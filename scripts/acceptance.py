#!/usr/bin/env python3
"""Live acceptance: signed IR v2 programs become systemd/Podman pods through Orqos's workload backend.

Requires root, systemd >= 254, Podman >= 5 and subordinate ID ranges for the `containers` user (see Orqos
docs/WORKLOAD_API.md). Starts its own Orqos (ORQOS_BACKENDS=workload) and Rezn on loopback ports with temporary
state, and removes everything it created, including after a failure.

    sudo python3 scripts/acceptance.py --orqos-bin ../orqos/target/debug/orqos --rezn-bin target/debug/rezn
"""
import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

from signed_program import generate_key, sign

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "docker.io/library/alpine:3.22"
OWNER = "dev.rezn.owner"


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def http(url, body=None, method=None, token=None, timeout=10):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        raw = response.read()
        return response.status, json.loads(raw) if raw else None


def wait(description, check, timeout=180, interval=0.5):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        # A podman command can fail while a container is being replaced: that is "not yet", not a failure.
        except (OSError, ValueError, AssertionError, KeyError, subprocess.CalledProcessError) as error:
            last = repr(error)
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {description}; last: {last}")


def podman(*args, check=True):
    result = subprocess.run(["podman", *args], check=check, capture_output=True, text=True, timeout=60)
    return result.stdout.strip()


# Answers every TCP connection on 8080 with the pod's hostname.
COMMAND = ["/bin/sh", "-c",
           "trap 'exit 0' TERM; (while true; do hostname | nc -l -p 8080; done) & while true; do sleep 1; done"]


def pod(level):
    return {"grace_seconds": 5, "containers": [{"name": "app", "image": IMAGE, "pull": "if_not_present",
                                                "command": COMMAND, "env": [{"name": "LEVEL", "value": level}]}]}


def replica_set(name, replicas, level):
    return {"kind": "replica_set", "name": name, "fields": {"replicas": replicas, "pod": pod(level)}}


def ordered_set(name, replicas):
    return {"kind": "ordered_replica_set", "name": name, "fields": {
        "replicas": replicas, "service": name, "pod_management": "ordered", "pod": pod("db")}}


def service(name, port=8080, target_port=8080, headless=False):
    return {"kind": "service", "name": name, "fields": {
        "target": name, "headless": headless,
        "ports": [{"name": "tcp", "port": port, "target_port": target_port}]}}


def resolve(container, name):
    """The IPv4 addresses `name` resolves to inside a container, or None."""
    result = subprocess.run(["podman", "exec", container, "getent", "ahostsv4", name],
                            capture_output=True, text=True, timeout=30)
    return None if result.returncode else {line.split()[0] for line in result.stdout.splitlines()}


REGISTRY = "localhost:5000"
REGISTRY_CONF = "/etc/containers/registries.conf.d/99-rezn-acceptance.conf"
PRIVATE = f"{REGISTRY}/acceptance/private:1"


def start_registry(workdir):
    """A local registry with htpasswd auth on plain HTTP, allowed for localhost:5000 by an acceptance-only drop-in."""
    os.makedirs(os.path.dirname(REGISTRY_CONF), exist_ok=True)
    Path(REGISTRY_CONF).write_text(f'[[registry]]\nlocation = "{REGISTRY}"\ninsecure = true\n')
    auth = Path(workdir) / "registry-auth"
    auth.mkdir(exist_ok=True)
    (auth / "htpasswd").write_text(podman("run", "--rm", "--entrypoint", "htpasswd", "docker.io/library/httpd:2.4",
                                          "-Bbn", "rezn", "s3cret") + "\n")
    podman("run", "-d", "--name", "rezn-acceptance-registry", "-p", "127.0.0.1:5000:5000", "-v", f"{auth}:/auth",
           "-e", "REGISTRY_AUTH=htpasswd", "-e", "REGISTRY_AUTH_HTPASSWD_REALM=acceptance",
           "-e", "REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd", "docker.io/library/registry:2")

    def up():
        try:
            urllib.request.urlopen(f"http://{REGISTRY}/v2/", timeout=2)
        except urllib.error.HTTPError as error:
            return error.code == 401
        return False
    wait("the registry", up, timeout=60)
    authfile = str(Path(workdir) / "push-auth.json")
    podman("login", "--authfile", authfile, "-u", "rezn", "-p", "s3cret", REGISTRY)
    podman("tag", IMAGE, PRIVATE)
    podman("push", "--authfile", authfile, PRIVATE)
    podman("rmi", PRIVATE)


def stop_registry():
    subprocess.run(["podman", "rm", "--force", "--ignore", "rezn-acceptance-registry"], capture_output=True)
    subprocess.run(["podman", "rmi", "--force", PRIVATE], capture_output=True)
    if os.path.exists(REGISTRY_CONF):
        os.remove(REGISTRY_CONF)


def docker_config(user, password):
    token = base64.b64encode(f"{user}:{password}".encode()).decode()
    return json.dumps({"auths": {REGISTRY: {"auth": token}}})


class Acceptance:
    def __init__(self, args, directory):
        self.args = args
        self.directory = Path(directory)
        self.deployment = "accept-" + secrets.token_hex(4)
        self.foreign = "accept-foreign-" + secrets.token_hex(4)
        self.key = generate_key(directory)
        self.token = secrets.token_hex(32)
        self.orqos_url = f"http://127.0.0.1:{free_port()}"
        self.rezn_url = f"http://127.0.0.1:{free_port()}"
        self.children = {}
        self.owner = None
        self.steps = []

    # Processes

    def start(self, name, binary, env, ready_url):
        assert name not in self.children, f"{name} is still running"
        log = (self.directory / f"{name}.log").open("ab")
        process = subprocess.Popen([str(binary)], cwd=self.directory, stdout=log, stderr=log, env={
            "PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "RUST_LOG": "info", **env})
        self.children[name] = process

        def ready():
            assert process.poll() is None, f"{name} exited; see {self.directory / (name + '.log')}"
            try:
                return http(ready_url)[0] == 200
            except urllib.error.HTTPError:
                return True
        wait(f"{name} startup", ready, 30)

    def stop(self, name):
        process = self.children.pop(name, None)
        if process and process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

    def start_orqos(self):
        self.start("orqos", self.args.orqos_bin, {
            "ORQOS_BACKENDS": "workload", "ORQOS_WORKLOAD_TOKEN": self.token,
            "BIND_ADDR": self.orqos_url.removeprefix("http://")}, self.orqos_url + "/healthz")

    def start_rezn(self):
        self.start("rezn", self.args.rezn_bin, {
            "STATE_DB_PATH": str(self.directory / "state"),
            "SECRETS_DB_PATH": str(self.directory / "secrets"),
            "REZN_AGE_IDENTITY": str(self.directory / "identity.txt"),
            "ORQOS_API_URL": self.orqos_url, "ORQOS_WORKLOAD_TOKEN": self.token,
            "BIND_ADDR": self.rezn_url.removeprefix("http://"), "RECONCILE_INTERVAL": "1"},
            self.rezn_url + "/status")
        owner = http(self.rezn_url + "/status")[1]["owner"]
        assert self.owner in (None, owner), "a restart changed Rezn's ownership identity"
        self.owner = owner

    # Rezn

    def apply(self, resources, deployment=None, namespace="acceptance"):
        deployment = deployment or self.deployment
        program = {"ir": "rezn.dev/v2", "deployment": deployment, "namespace": namespace, "resources": resources}
        code, accepted = http(self.rezn_url + "/apply", {
            "name": deployment, "instruction_wrapper": sign(program, self.key)})
        assert code == 202 and accepted["stored"] is True, accepted
        return accepted["revision"]

    def owned_networks(self):
        _, networks = http(self.orqos_url + "/workload/networks", token=self.token)
        return networks

    def status(self):
        return http(self.rezn_url + "/status")[1]

    def settled(self, revision):
        def check():
            status = self.status()
            return status if (status["converged"] and status["observation"] == "fresh"
                              and status["desired_revision"] == revision
                              and status["observed_revision"] == revision) else None
        return wait(f"convergence of revision {revision}", check)

    def pods(self, status, resource="web", deployment=None):
        return {p["ordinal"]: p for w in status["workloads"]
                if w["deployment"] == (deployment or self.deployment) and w["resource"] == resource
                for p in w["pods"]}

    @staticmethod
    def container(pod):
        return f"orqos-{pod['id']}-app"

    def container_ids(self, status, resource="web"):
        return {n: podman("inspect", "--format", "{{.Id}}", self.container(p))
                for n, p in self.pods(status, resource).items()}

    def owned_podman_pods(self):
        return podman("pod", "ps", "-q", "--filter", f"label={OWNER}={self.owner}").split()

    def record(self, description):
        self.steps.append(description)
        print(f"PASS {description}", flush=True)

    def foreign_intact(self):
        code, pod = http(f"{self.orqos_url}/workload/pods/{self.foreign}", token=self.token)
        assert code == 200 and pod["state"] == "running", pod

    # Scenario

    def execute(self):
        podman("pull", "-q", IMAGE)
        self.start_orqos()
        self.start_rezn()
        http(f"{self.orqos_url}/workload/pods/{self.foreign}", method="PUT", token=self.token, timeout=700, body={
            "hostname": "foreign", "labels": {"example.org/app": "foreign"},
            "containers": [{"name": "app", "image": IMAGE, "command": ["sleep", "infinity"]}]})

        status = self.settled(self.apply([replica_set("web", 2, "a")]))
        pods = self.pods(status)
        assert sorted(pods) == [0, 1]
        for n, pod in pods.items():
            assert podman("exec", self.container(pod), "hostname") == f"web-{n}"
            assert podman("exec", self.container(pod), "printenv", "LEVEL") == "a"
        first = self.container_ids(status)
        self.record("a signed IR v2 replica set runs as one systemd/Podman pod per replica")

        status = self.settled(self.apply([replica_set("web", 2, "a")]))
        assert self.container_ids(status) == first
        self.record("re-applying an unchanged program keeps every container")

        status = self.settled(self.apply([replica_set("web", 3, "a")]))
        three = self.container_ids(status)
        assert sorted(three) == [0, 1, 2] and {n: three[n] for n in (0, 1)} == first
        status = self.settled(self.apply([replica_set("web", 1, "a")]))
        assert self.container_ids(status) == {0: first[0]}
        self.record("scaling 2 -> 3 -> 1 adds and removes the highest ordinals only")

        status = self.settled(self.apply([replica_set("web", 2, "a")]))
        before = self.container_ids(status)
        names = {n: self.container(p) for n, p in self.pods(status).items()}
        revision = self.apply([replica_set("web", 2, "b")])
        changed_at = {}
        while True:
            running = 0
            for n, name in names.items():
                state = podman("inspect", "--format", "{{.Id}} {{.State.Running}}", name, check=False).split()
                if state and state[1] == "true":
                    running += 1
                    if state[0] != before[n]:
                        changed_at.setdefault(n, time.monotonic())
            assert running >= 1, "a rolling replacement stopped both replicas at once"
            status = self.status()
            if status["converged"] and status["observed_revision"] == revision:
                break
            time.sleep(0.2)
        # A replica replaced between this iteration's inspection and the status call is seen only now.
        for n, name in names.items():
            if podman("inspect", "--format", "{{.Id}}", name) != before[n]:
                changed_at.setdefault(n, time.monotonic())
        assert set(changed_at) == {0, 1} and changed_at[1] < changed_at[0], changed_at
        for pod in self.pods(status).values():
            assert podman("exec", self.container(pod), "printenv", "LEVEL") == "b"
        self.record("a changed spec replaces replicas one at a time, highest ordinal first")

        current = self.container_ids(status)
        limited = replica_set("web", 2, "c")
        limited["fields"]["pod"]["containers"][0]["env"] = [{"name": "IP", "pod_ip": True}]
        revision = self.apply([limited])

        def unexecuted():
            s = self.status()
            return s if s["observed_revision"] == revision and s["unexecuted"] else None
        status = wait("the unexecuted report", unexecuted)
        assert not status["converged"] and status["unexecuted"][0]["deployment"] == self.deployment
        assert "pod_ip: pod IP references are not executed yet" in \
            status["unexecuted"][0]["reasons"][0], status
        time.sleep(3)
        assert self.container_ids(self.status()) == current
        self.record("a program needing unsupported features is reported with reasons and changes nothing")

        status = self.settled(self.apply([replica_set("web", 2, "b")]))
        current = self.container_ids(status)
        revision = status["desired_revision"]
        self.stop("rezn")
        self.start_rezn()
        status = self.settled(revision)
        assert self.container_ids(status) == current
        self.record("a restarted Rezn keeps its identity and changes no running container")

        victim = self.pods(status)[0]
        podman("kill", self.container(victim))

        def restarted():
            s = self.status()
            pod = self.pods(s)[0]
            return s if pod["state"] == "running" and pod["containers"][0]["restarts"] >= 1 else None
        wait("systemd to restart the killed container", restarted)
        status = self.settled(revision)
        assert self.pods(status)[0]["id"] == victim["id"]
        self.record("a killed container is restarted by systemd without Rezn replacing the pod")

        self.services()
        self.storage()
        self.configuration()
        self.certificates()
        self.probes()
        self.pod_lifecycle()
        self.resources()
        self.registry()

        networks = {n["podman_name"] for n in self.owned_networks()}
        assert len(networks) == 2, networks
        self.settled(self.apply([], deployment=self.deployment + "-x", namespace="isolated"))
        status = self.settled(self.apply([]))
        assert not self.pods(status)
        wait("owned Podman pods to disappear", lambda: not self.owned_podman_pods(), 60)
        wait("namespace networks to disappear", lambda: not self.owned_networks(), 60)
        for network in networks:
            assert subprocess.run(["podman", "network", "exists", network]).returncode != 0, network
        self.foreign_intact()
        self.record("empty programs remove every owned pod and then the namespace networks; a foreign Orqos pod "
                    "is untouched")
        return self.steps

    def storage(self):
        name = self.deployment + "-s"

        def program(replicas, level="a"):
            return [
                {"kind": "volume_claim", "name": "shared", "fields": {"bytes": 1048576}},
                {"kind": "ordered_replica_set", "name": "db", "fields": {
                    "replicas": replicas, "service": "db", "pod_management": "parallel",
                    "volume_claims": [{"name": "data", "bytes": 1048576}],
                    "pod": {"grace_seconds": 5, "user": 1000, "group": 1000, "fs_group": 2000,
                            "fs_group_policy": "on_root_mismatch", "run_as_non_root": True,
                            "volumes": [{"name": "data", "claim": "data"}, {"name": "shared", "claim": "shared"},
                                        {"name": "tmp", "scratch": {"medium": "memory", "bytes": 1048576}}],
                            "containers": [{"name": "app", "image": IMAGE, "pull": "if_not_present",
                                            "command": COMMAND, "env": [{"name": "LEVEL", "value": level}],
                                            "security": {"read_only_root": True,
                                                         "allow_privilege_escalation": False,
                                                         "drop_capabilities": ["all"]},
                                            "mounts": [{"volume": "data", "path": "/data"},
                                                       {"volume": "shared", "path": "/shared"},
                                                       {"volume": "tmp", "path": "/tmp"}]}]}}},
                {"kind": "service", "name": "db", "fields": {"target": "db", "headless": True, "ports": []}},
            ]

        def apply(resources):
            return self.settled(self.apply(resources, deployment=name, namespace="storage"))

        def replicas(status):
            return self.pods(status, "db", name)

        def run(pod, script):
            return podman("exec", self.container(pod), "sh", "-c", script)

        def volumes(status):
            return {(v["claim"], v["ordinal"]): v for v in status["volumes"] if v["deployment"] == name}

        status = apply(program(2))
        pods = replicas(status)
        for n, pod in pods.items():
            run(pod, f"echo db-{n} > /data/marker && echo {n} >> /shared/log && echo scratch > /tmp/t")
        ident = run(pods[0], "id; grep CapEff /proc/self/status; touch /x 2>/dev/null || echo read-only")
        assert "uid=1000" in ident and ",2000" in ident.split("groups=")[1] and "CapEff:\t0000000000000000" in ident \
            and "read-only" in ident, ident
        own = volumes(status)
        assert len(own) == 3 and all(v["desired"] and v["exists"] for v in own.values()), own
        marker = Path(podman("volume", "inspect", f"orqos-vol-{own[('data', 1)]['id']}", "--format",
                             "{{.Mountpoint}}")) / "marker"
        stat = marker.stat()
        assert (stat.st_uid, stat.st_gid) == (1000, 2000), (stat.st_uid, stat.st_gid)
        assert sorted(run(pods[1], "cat /shared/log").split()) == ["0", "1"]
        self.record("pods run as their user with fs_group-owned volumes, read-only and without capabilities; "
                    "files keep container IDs on disk and a shared claim is shared")

        status = apply(program(1))
        retained = volumes(status)[("data", 1)]
        assert not retained["desired"] and retained["retain"] and retained["exists"], retained
        status = apply(program(2))
        assert run(replicas(status)[1], "cat /data/marker") == "db-1"
        self.record("scaling 2 -> 1 -> 2 brings replica 1 back with its own volume")

        status = apply(program(2, "b"))
        for n, pod in replicas(status).items():
            assert run(pod, "cat /data/marker; printenv LEVEL; ls /tmp | wc -l").split() == [f"db-{n}", "b", "0"]
        self.record("replacing every replica keeps each one's volume and starts its scratch volume empty")

        status = apply([])
        assert not replicas(status)
        own = volumes(status)
        assert len(own) == 3 and all(not v["desired"] and v["retain"] for v in own.values()), own
        status = apply(program(2))
        assert run(replicas(status)[0], "cat /data/marker") == "db-0"
        self.settled(self.apply([], deployment=name, namespace="storage"))
        self.record("removing the deployment keeps its volumes, and applying it again reuses them")

    def configuration(self):
        name = self.deployment + "-k"
        namespace = "config"
        password = "pw-1 $HOME \"quoted\""

        def program(settings, bystander_level="a"):
            return [
                {"kind": "config", "name": "settings", "fields": {"data": {"app.json": settings, "ca.crt": "CA"}}},
                {"kind": "secret", "name": "db", "fields": {"type": "opaque", "keys": ["password"],
                                                            "source": {"store": {}}}},
                {"kind": "secret", "name": "app", "fields": {"type": "opaque", "keys": ["token"], "source": {
                    "random": {"token": {"length": 32, "alphabet": "hex"}}}}},
                {"kind": "replica_set", "name": "api", "fields": {"replicas": 2, "pod": {
                    "grace_seconds": 5, "user": 1000, "group": 1000, "fs_group": 2000,
                    "volumes": [
                        {"name": "conf", "config": {"name": "settings", "items": [
                            {"key": "app.json", "path": "conf/app.json"}, {"key": "ca.crt", "path": "ca.crt"}]}},
                        {"name": "creds", "secret": {"name": "db", "mode": 0o440}}],
                    "containers": [{"name": "app", "image": IMAGE, "pull": "if_not_present", "command": COMMAND,
                                    "env": [{"name": "DB_PASSWORD", "secret": {"name": "db", "key": "password"}},
                                            {"name": "TOKEN", "secret": {"name": "app", "key": "token"}}],
                                    "security": {"read_only_root": True},
                                    "mounts": [{"volume": "conf", "path": "/etc/api"},
                                               {"volume": "conf", "path": "/opt/app.json", "sub_path": "conf/app.json"},
                                               {"volume": "creds", "path": "/run/creds"}]}]}}},
                {"kind": "replica_set", "name": "bystander", "fields": {"replicas": 1, "pod": pod(bystander_level)}},
            ]

        def apply(resources):
            return self.settled(self.apply(resources, deployment=name, namespace=namespace))

        def api(status):
            return self.pods(status, "api", name)

        def ids(status, resource):
            return {n: podman("inspect", "--format", "{{.Id}}", self.container(p))
                    for n, p in self.pods(status, resource, name).items()}

        def run(replica, script):
            return podman("exec", self.container(replica), "sh", "-c", script)

        def put_secret(secret, data):
            try:
                return http(f"{self.rezn_url}/namespaces/{namespace}/secrets/{secret}", {"data": data}, method="PUT")
            except urllib.error.HTTPError as error:
                return error.code, error.read().decode()

        def delete_secret(secret):
            try:
                return http(f"{self.rezn_url}/namespaces/{namespace}/secrets/{secret}", method="DELETE")[0]
            except urllib.error.HTTPError as error:
                return error.code

        def owned_secrets():
            _, listed = http(self.orqos_url + "/workload/secrets", token=self.token)
            return [s for s in listed if s["labels"].get(OWNER) == self.owner]

        try:
            self.apply(program('{"v":1}'), deployment=name, namespace=namespace)
            raise AssertionError("a program missing a stored secret was accepted")
        except urllib.error.HTTPError as error:
            body = error.read().decode()
            assert error.code == 400 and 'key "password" of secret "db" is not in the secret store' in body, body
        code, body = put_secret("db", {"password": password})
        assert code == 200 and body["keys"][0]["key"] == "password", body
        status = apply(program('{"v":1}'))
        pods = api(status)
        tokens = set()
        for replica in pods.values():
            out = run(replica, "ls /etc/api | tr '\\n' ' '; echo; cat /etc/api/conf/app.json; echo; cat /opt/app.json; echo; "
                           "stat -c '%u %g %a' /run/creds /run/creds/password; cat /run/creds/password; echo; "
                           "printf '%s\\n' \"$DB_PASSWORD\" \"$TOKEN\"; touch /etc/api/x 2>/dev/null || echo read-only")
            lines = out.splitlines()
            assert lines[0].split() == ["ca.crt", "conf"] and lines[1] == lines[2] == '{"v":1}', lines
            assert lines[3:5] == ["0 2000 550", "0 2000 440"] and lines[5] == password, lines
            assert lines[6] == password and len(lines[7]) == 32 and lines[8] == "read-only", lines
            tokens.add(lines[7])
        assert len(tokens) == 1, tokens
        self.record("config and secret files reach pods as root:<fs_group> read-only directories and files with "
                    "their modes, and stored and generated secrets become environment variables")

        first, bystander = ids(status, "api"), ids(status, "bystander")
        status = apply(program('{"v":1}'))
        assert ids(status, "api") == first and run(api(status)[0], 'echo "$TOKEN"') in tokens
        status = apply(program('{"v":2}'))
        assert ids(status, "bystander") == bystander
        changed = ids(status, "api")
        assert all(changed[n] != first[n] for n in first), (first, changed)
        assert all(run(p, "cat /etc/api/conf/app.json") == '{"v":2}' for p in api(status).values())
        assert run(api(status)[0], 'echo "$TOKEN"') in tokens
        self.record("a random secret is generated once and kept; a changed config replaces only the pods that use it")

        code, _ = put_secret("db", {"password": password})
        assert code == 200
        time.sleep(4)
        assert ids(self.status(), "api") == changed
        code, _ = put_secret("db", {"password": "pw-2"})
        assert code == 200

        def rotated():
            s = self.status()
            return s if s["converged"] and all(run(p, 'echo "$DB_PASSWORD"') == "pw-2"
                                               for p in api(s).values()) else None
        status = wait("the new secret value to reach every replica", rotated)
        assert ids(status, "bystander") == bystander
        self.record("storing the same secret value changes nothing; a new value replaces only the pods that use it")

        code, body = put_secret("app", {"token": "mine"})
        assert code == 409 and "generated by Rezn" in body, (code, body)
        assert delete_secret("db") == 409
        token = next(iter(tokens))
        for url, auth in [(self.rezn_url + "/status", None), (self.rezn_url + "/state", None),
                          (f"{self.rezn_url}/namespaces/{namespace}/secrets", None),
                          (self.orqos_url + "/workload/pods", self.token),
                          (self.orqos_url + "/workload/secrets", self.token)]:
            text = json.dumps(http(url, token=auth)[1])
            assert "pw-2" not in text and token not in text and "pw-1" not in text, url
        assert len(owned_secrets()) == 3, owned_secrets()
        self.record("generated secrets cannot be overwritten, declared ones cannot be deleted, and no value appears "
                    "in Rezn's or Orqos's responses")

        apply([])
        wait("owned Orqos secrets to disappear", lambda: not owned_secrets(), 60)
        _, listed = http(f"{self.rezn_url}/namespaces/{namespace}/secrets")
        assert sorted(s["name"] for s in listed) == ["app", "db"], listed
        assert delete_secret("db") == 200 and delete_secret("app") == 200
        self.record("removing the deployment removes its Orqos secrets and keeps stored values until deleted")

    def certificates(self):
        name = self.deployment + "-t"
        namespace = "tls"
        sleep = "trap 'exit 0' TERM; while true; do sleep 1; done"

        def program(names):
            return [
                {"kind": "secret", "name": "ca", "fields": {"type": "tls", "keys": ["tls.crt", "tls.key"],
                                                            "source": {"ca": {"common_name": "acceptance CA",
                                                                              "days": 3650}}}},
                {"kind": "secret", "name": "web-tls", "fields": {"type": "tls", "keys": ["tls.crt", "tls.key"],
                                                                 "source": {"certificate": {
                                                                     "ca": "ca", "dns_names": names, "days": 90}}}},
                {"kind": "replica_set", "name": "web", "fields": {"replicas": 1, "pod": {
                    "grace_seconds": 5, "fs_group": 2000,
                    "volumes": [{"name": "tls", "secret": {"name": "web-tls", "mode": 0o440}}],
                    "containers": [{"name": "app", "image": IMAGE, "pull": "if_not_present",
                                    "command": ["/bin/sh", "-c",
                                                "apk add --no-cache openssl >/tmp/apk.log 2>&1 && exec openssl "
                                                "s_server -quiet -accept 8443 -cert /run/tls/tls.crt "
                                                "-key /run/tls/tls.key"],
                                    "ports": [{"name": "https", "port": 8443}],
                                    "mounts": [{"volume": "tls", "path": "/run/tls"}]}]}}},
                {"kind": "service", "name": "web", "fields": {
                    "target": "web", "ports": [{"name": "https", "port": 8443, "target_port": "https"}]}},
                {"kind": "replica_set", "name": "client", "fields": {"replicas": 1, "pod": {
                    "grace_seconds": 5,
                    "volumes": [{"name": "ca", "secret": {"name": "ca",
                                                          "items": [{"key": "tls.crt", "path": "ca.crt"}]}}],
                    "containers": [{"name": "app", "image": IMAGE, "pull": "if_not_present",
                                    "command": ["/bin/sh", "-c",
                                                f"apk add --no-cache openssl >/tmp/apk.log 2>&1; {sleep}"],
                                    "mounts": [{"volume": "ca", "path": "/run/ca"}]}]}}},
            ]

        def apply(names):
            return self.settled(self.apply(program(names), deployment=name, namespace=namespace))

        def one(status, resource):
            return self.pods(status, resource, name)[0]

        def run(replica, script, check=True):
            return subprocess.run(["podman", "exec", self.container(replica), "sh", "-c", script],
                                  capture_output=True, text=True, timeout=60, check=check)

        def handshake(client, host):
            return run(client, f"echo | openssl s_client -connect web.tls.svc:8443 -servername {host} "
                               f"-CAfile /run/ca/ca.crt -verify_hostname {host} -verify_return_error 2>&1",
                       check=False)

        def host_openssl(*args, stdin=None):
            return subprocess.run(["openssl", *args], input=stdin, capture_output=True, text=True,
                                  timeout=30)

        status = apply(["web", "web.tls.svc"])
        web, client = one(status, "web"), one(status, "client")

        def trusted():
            result = handshake(client, "web.tls.svc")
            return result.returncode == 0 and "Verify return code: 0 (ok)" in result.stdout
        wait("a verified TLS handshake by service name", trusted, timeout=240, interval=2)
        refused = handshake(client, "other.tls.svc")
        assert refused.returncode != 0, refused.stdout
        leaf = run(web, "cat /run/tls/tls.crt").stdout
        ca = run(client, "cat /run/ca/ca.crt").stdout
        with tempfile.NamedTemporaryFile("w", suffix=".pem") as ca_file:
            ca_file.write(ca)
            ca_file.flush()
            verified = host_openssl("verify", "-CAfile", ca_file.name, "-verify_hostname", "web.tls.svc",
                                    stdin=leaf)
            assert verified.returncode == 0, verified.stdout + verified.stderr
        sans = host_openssl("x509", "-noout", "-ext", "subjectAltName", stdin=leaf).stdout
        assert "DNS:web, DNS:web.tls.svc" in sans, sans
        assert run(client, "ls -A /run/ca").stdout.split() == ["ca.crt"]
        stat = run(web, "stat -c '%u %g %a' /run/tls/tls.key").stdout.strip()
        assert stat == "0 2000 440", stat
        _, objects = http(self.orqos_url + "/workload/secrets", token=self.token)
        mine = [o for o in objects if o["labels"].get("dev.rezn.deployment") == name]
        keys = {o["labels"]["dev.rezn.resource"]: o["keys"] for o in mine}
        assert keys == {"ca": ["tls.crt"], "web-tls": ["tls.crt", "tls.key"]}, keys
        self.record("a generated CA signs a certificate for a service's names: a client trusting only the CA's "
                    "certificate completes a verified TLS handshake by name, and the CA key never leaves Rezn")

        def serial(replica):
            return host_openssl("x509", "-noout", "-serial",
                                stdin=run(replica, "cat /run/tls/tls.crt").stdout).stdout.strip()
        first_serial = serial(web)
        ids = {r: podman("inspect", "--format", "{{.Id}}", self.container(one(status, r))) for r in ("web", "client")}
        status = apply(["web", "web.tls.svc"])
        assert serial(one(status, "web")) == first_serial
        assert {r: podman("inspect", "--format", "{{.Id}}", self.container(one(status, r)))
                for r in ("web", "client")} == ids
        status = apply(["web", "web.tls", "web.tls.svc"])
        web = one(status, "web")
        assert podman("inspect", "--format", "{{.Id}}", self.container(web)) != ids["web"]
        assert podman("inspect", "--format", "{{.Id}}", self.container(one(status, "client"))) == ids["client"]
        assert serial(web) != first_serial
        wait("a verified handshake for the added name", lambda: handshake(client, "web.tls").returncode == 0,
             timeout=240, interval=2)
        self.record("an unchanged re-apply keeps the certificate and every container; adding a name reissues it "
                    "from the same CA and replaces only the server")

        _, listed = http(f"{self.rezn_url}/namespaces/{namespace}/secrets")
        by_name = {s["name"]: s for s in listed}
        assert by_name["ca"]["certificate"]["kind"] == "ca" and by_name["web-tls"]["certificate"]["renew_after"]
        assert "BEGIN" not in json.dumps(listed)
        try:
            http(f"{self.rezn_url}/namespaces/{namespace}/secrets/ca", {"data": {"tls.crt": "x"}}, method="PUT")
            raise AssertionError("a generated CA was overwritten")
        except urllib.error.HTTPError as error:
            assert error.code == 409, error.code
        self.settled(self.apply([], deployment=name, namespace=namespace))
        for secret in ("ca", "web-tls"):
            assert http(f"{self.rezn_url}/namespaces/{namespace}/secrets/{secret}", method="DELETE")[0] == 200
        self.record("the listing shows certificate validity without values, generated certificates cannot be set, "
                    "and stored certificates outlive their deployment until deleted")

    def probes(self):
        name = self.deployment + "-p"
        namespace = "probes"
        responder = ("trap 'exit 0' TERM; while true; do if [ -f /tmp/ready ]; then s='200 OK'; "
                     "else s='503 Service Unavailable'; fi; printf \"HTTP/1.1 $s\\r\\nContent-Length: 0\\r\\n"
                     "Connection: close\\r\\n\\r\\n\" | nc -l -p 8080; done")

        def template(level):
            return {"grace_seconds": 2, "containers": [{
                "name": "app", "image": IMAGE, "pull": "if_not_present", "command": ["/bin/sh", "-c", responder],
                "env": [{"name": "LEVEL", "value": level}], "ports": [{"name": "http", "port": 8080}],
                "startup": {"exec": ["test", "-f", "/tmp/started"], "period_seconds": 1, "failure_threshold": 600},
                "readiness": {"http": {"path": "/ready", "port": "http", "scheme": "http"}, "period_seconds": 1,
                              "failure_threshold": 1},
                "liveness": {"exec": ["/bin/sh", "-c", "test ! -f /tmp/dead"], "period_seconds": 1,
                             "failure_threshold": 2}}]}

        def web(level):
            return {"kind": "replica_set", "name": "web", "fields": {"replicas": 2, "pod": template(level)}}

        def apply(resources):
            return self.apply(resources, deployment=name, namespace=namespace)

        def replicas(status, resource="web"):
            return self.pods(status, resource, name)

        def make_ready(replica):
            podman("exec", self.container(replica), "touch", "/tmp/started", "/tmp/ready")

        def container_id(replica):
            return podman("inspect", "--format", "{{.Id}}", self.container(replica))

        def workload(status, resource="web"):
            return next(w for w in status["workloads"] if w["deployment"] == name and w["resource"] == resource)

        revision = apply([web("a")])

        def running_unready():
            s = self.status()
            pods = replicas(s)
            return s if (s["observed_revision"] == revision and len(pods) == 2
                         and all(p["state"] == "running" for p in pods.values())) else None
        status = wait("both replicas running", running_unready)
        assert not status["converged"] and workload(status)["ready_replicas"] == 0, workload(status)
        assert all(not p["ready"] and not p["containers"][0]["started"] for p in replicas(status).values())
        for replica in replicas(status).values():
            make_ready(replica)
        status = self.settled(revision)
        assert workload(status)["ready_replicas"] == 2
        self.record("replicas that run but have not passed their startup and readiness probes are not ready, and "
                    "the deployment converges once they pass (an HTTP probe from inside the pod, by port name)")

        victim = replicas(status)[0]
        before = container_id(victim)
        podman("exec", self.container(victim), "touch", "/tmp/dead")

        def restarted():
            # The probe restart is recorded before `podman stop` finishes: wait for the new container instance.
            s = self.status()
            replica = replicas(s)[0]
            now = podman("inspect", "--format", "{{.Id}}", self.container(replica), check=False)
            return s if (replica["containers"][0]["probe_restarts"] >= 1 and replica["state"] == "running"
                         and now and now != before) else None
        status = wait("a liveness restart", restarted, timeout=60)
        replica = replicas(status)[0]
        assert replica["id"] == victim["id"] and container_id(replica) != before and not replica["ready"], replica
        assert not status["converged"]
        make_ready(replica)
        status = self.settled(revision)
        self.record("failing liveness restarts the container, not the pod, and it is unready until its probes pass "
                    "again")

        old = {n: container_id(p) for n, p in replicas(status).items()}
        revision = apply([web("b")])

        def first_replaced():
            s = self.status()
            pods = replicas(s)
            if s["observed_revision"] != revision or 1 not in pods or pods[1]["state"] != "running":
                return None
            return s if container_id(pods[1]) != old[1] else None
        status = wait("replica 1 to be replaced first", first_replaced)
        time.sleep(5)
        status = self.status()
        pods = replicas(status)
        assert container_id(pods[0]) == old[0], "replica 0 was replaced while replica 1 was not ready"
        rollout = workload(status)["rollout"]
        assert rollout and rollout["waiting_for"] == [pods[1]["id"]], rollout
        make_ready(pods[1])

        def second_replaced():
            s = self.status()
            pods = replicas(s)
            return s if 0 in pods and pods[0]["state"] == "running" and container_id(pods[0]) != old[0] else None
        status = wait("replica 0 to be replaced after replica 1 is ready", second_replaced)
        make_ready(replicas(status)[0])
        status = self.settled(revision)
        assert all(podman("exec", self.container(p), "printenv", "LEVEL") == "b" for p in replicas(status).values())
        assert workload(status)["rollout"] is None
        self.record("a rolling change waits for each new replica to be ready before replacing the next, and reports "
                    "what it waits for")

        db = {"kind": "ordered_replica_set", "name": "db", "fields": {
            "replicas": 2, "service": "db", "pod_management": "ordered", "pod": template("db")}}
        headless = {"kind": "service", "name": "db", "fields": {
            "target": "db", "headless": True, "ports": [{"name": "http", "port": 8080, "target_port": "http"}]}}
        revision = apply([web("b"), db, headless])
        status = wait("db-0 running", lambda: (lambda s: s if 0 in replicas(s, "db") and
                                               replicas(s, "db")[0]["state"] == "running" else None)(self.status()))
        time.sleep(5)
        assert 1 not in replicas(self.status(), "db"), "db-1 was created before db-0 was ready"
        make_ready(replicas(status, "db")[0])
        status = wait("db-1 running", lambda: (lambda s: s if 1 in replicas(s, "db") and
                                               replicas(s, "db")[1]["state"] == "running" else None)(self.status()))
        make_ready(replicas(status, "db")[1])
        self.settled(revision)
        self.record("an ordered replica set creates each replica only once the previous one is ready")
        self.settled(apply([]))

    def pod_lifecycle(self):
        name = self.deployment + "-w"
        namespace = "pods"
        work = [{"volume": "work", "path": "/work"}]
        flaky = "n=$(cat /work/n 2>/dev/null || echo 0); echo $((n+1)) > /work/n; [ $n -ge 2 ]"
        loop = "trap 'exit 0' TERM; while true; do sleep 1; done"

        def worker(level):
            return {"kind": "replica_set", "name": "worker", "fields": {"replicas": 1, "pod": {
                "grace_seconds": 2,
                "volumes": [{"name": "work", "scratch": {"medium": "memory", "bytes": 1048576}}],
                "init": [
                    {"name": "one", "image": IMAGE, "command": ["/bin/sh", "-c", "echo -n 1 > /work/order"],
                     "mounts": work},
                    {"name": "two", "image": IMAGE, "command": ["/bin/sh", "-c", "echo -n 2 >> /work/order"],
                     "mounts": work},
                    {"name": "flaky", "image": IMAGE, "command": ["/bin/sh", "-c", flaky], "mounts": work}],
                "sidecars": [{"name": "side", "image": IMAGE, "mounts": work,
                              "command": ["/bin/sh", "-c", f"sleep 3; date +%s > /work/token; {loop}"],
                              "startup": {"exec": ["test", "-s", "/work/token"], "period_seconds": 1,
                                          "failure_threshold": 60}}],
                "containers": [{"name": "app", "image": IMAGE, "mounts": work,
                                "env": [{"name": "LEVEL", "value": level}],
                                "command": ["/bin/sh", "-c", "cat /work/order > /work/main-saw; test -s /work/token && "
                                            f"echo yes > /work/main-token; {loop}"]}]}}}

        def check(status):
            replica = self.pods(status, "worker", name)[0]
            out = podman("exec", self.container(replica), "sh", "-c",
                         "cat /work/order; echo; cat /work/n; cat /work/main-saw; echo; cat /work/main-token")
            assert out.split() == ["12", "3", "12", "yes"], out
            kinds = [(c["name"], c["kind"]) for c in replica["containers"]]
            assert kinds == [("one", "init"), ("two", "init"), ("flaky", "init"), ("side", "sidecar"),
                             ("app", "container")], kinds
            assert all(c["sub_state"] == "exited" for c in replica["containers"] if c["kind"] == "init")
            assert replica["ready"] and all(c["ready"] for c in replica["containers"] if c["kind"] != "init")
            return replica

        status = self.settled(self.apply([worker("a")], deployment=name, namespace=namespace))
        first = check(status)
        self.record("init containers run once, in order, and a failing one is retried until it succeeds; the main "
                    "container starts only after its sidecar's startup probe passes; status lists every container "
                    "with its kind")
        status = self.settled(self.apply([worker("b")], deployment=name, namespace=namespace))
        replica = check(status)
        assert replica["id"] == first["id"]
        assert podman("exec", self.container(replica), "printenv", "LEVEL") == "b"
        self.record("a changed pod runs its init containers and gated start again, from empty scratch volumes")
        self.settled(self.apply([], deployment=name, namespace=namespace))

    def resources(self):
        namespace = "resources"
        limited = replica_set("limited", 1, "a")
        limited["fields"]["pod"]["containers"][0]["resources"] = {
            "cpu_limit_millis": 500, "cpu_request_millis": 250,
            "memory_limit_bytes": 64 << 20, "memory_request_bytes": 32 << 20}
        name = self.deployment + "-r"
        status = self.settled(self.apply([limited], deployment=name, namespace=namespace))
        replica = self.pods(status, "limited", name)[0]
        read = podman("exec", self.container(replica), "sh", "-c",
                      "cd /sys/fs/cgroup && cat memory.max memory.swap.max memory.low cpu.max cpu.weight").splitlines()
        assert read[:4] == ["67108864", "0", "33554432", "50000 100000"] and read[4] != "100", read
        self.record("limits become the container's cgroup limits (memory.max without swap, cpu.max) and requests its "
                    "memory protection and CPU weight")

        _, capacity = http(self.orqos_url + "/workload/capacity", token=self.token)
        allocatable = capacity["cpu_millis"] - 250
        share = allocatable * 6 // 10

        def cpu(replicas, millis):
            workload = replica_set("busy", replicas, "a")
            workload["fields"]["pod"]["containers"][0]["resources"] = {"cpu_request_millis": millis}
            return [workload]
        first, second = self.deployment + "-ra", self.deployment + "-rb"
        self.settled(self.apply(cpu(1, share), deployment=first, namespace="admit-a"))
        try:
            self.apply(cpu(1, share), deployment=second, namespace="admit-b")
            raise AssertionError("a revision over the host's allocatable CPU was accepted")
        except urllib.error.HTTPError as error:
            body = error.read().decode()
            assert error.code == 400 and f"CPU: {2 * share + 250}m requested of {allocatable}m allocatable" in body, body
        self.settled(self.apply(cpu(1, share // 6), deployment=first, namespace="admit-a"))
        status = self.settled(self.apply(cpu(1, share), deployment=second, namespace="admit-b"))
        assert status["resources"]["requested"]["cpu_millis"] == share // 6 + share + 250, status["resources"]
        assert status["resources"]["allocatable"]["cpu_millis"] == allocatable, status["resources"]
        self.record("a revision whose requests exceed the host's allocatable CPU, with the other deployments', is "
                    "refused with the figures; shrinking another deployment makes room, and status shows the totals")
        for deployment, ns in ((first, "admit-a"), (second, "admit-b"), (name, namespace)):
            self.settled(self.apply([], deployment=deployment, namespace=ns))

    def registry(self):
        start_registry(self.directory)
        name = self.deployment + "-g"
        namespace = "registry"

        def program(pull="always", image=PRIVATE):
            private = replica_set("private", 1, "a")
            private["fields"]["pod"]["image_pull_secrets"] = ["registry"]
            private["fields"]["pod"]["containers"][0].update({"image": image, "pull": pull})
            return [{"kind": "secret", "name": "registry", "fields": {
                "type": "opaque", "keys": [".dockerconfigjson"], "source": {"store": {}}}}, private]

        def put_credentials(password):
            return http(f"{self.rezn_url}/namespaces/{namespace}/secrets/registry",
                        {"data": {".dockerconfigjson": docker_config("rezn", password)}}, method="PUT")[0]

        try:
            self.apply(program(), deployment=name, namespace=namespace)
            raise AssertionError("a program missing its registry credentials was accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 400 and 'key ".dockerconfigjson" of secret "registry"' in error.read().decode()
        assert put_credentials("wrong") == 200
        revision = self.apply(program(), deployment=name, namespace=namespace)

        def refused():
            s = self.status()
            # Podman words it "unauthorized", "authentication required" or "denied".
            refused = ("unauthorized", "authentication required", "denied")
            return s if (s["observed_revision"] == revision
                         and any(w in e.lower() for e in s["errors"] for w in refused)) else None
        status = wait("the unauthorized pull to be reported", refused, timeout=120)
        assert not self.pods(status, "private", name), "a pod ran without its image"
        self.record("a pull with wrong registry credentials is reported in status and runs nothing")

        assert put_credentials("s3cret") == 200
        status = self.settled(revision)
        replica = self.pods(status, "private", name)[0]
        image_id = podman("image", "inspect", "--format", "{{.Id}}", PRIVATE)
        assert replica["containers"][0]["image_id"] == image_id, (replica["containers"][0], image_id)
        self.record("storing the right credentials lets the pod pull and run its private image (pull: always)")

        revision = self.apply(program(pull="never", image=f"{REGISTRY}/acceptance/absent:1"), deployment=name,
                              namespace=namespace)

        def never():
            s = self.status()
            return s if (s["observed_revision"] == revision
                         and any("pull policy never" in e for e in s["errors"])) else None
        wait("the absent image to be reported", never, timeout=120)
        self.record("pull: never with an absent image is reported and leaves the running pod alone")
        self.settled(self.apply([], deployment=name, namespace=namespace))
        http(f"{self.rezn_url}/namespaces/{namespace}/secrets/registry", method="DELETE")

    def services(self):
        web_service = service("web")
        status = self.settled(self.apply([replica_set("web", 2, "b"), web_service]))
        pods = self.pods(status)
        client = self.container(pods[0])
        assert pods[0]["aliases"] == ["web", "web.acceptance", "web.acceptance.svc"], pods[0]
        both = resolve(client, "web")
        assert both and len(both) == 2, both
        assert resolve(client, "web.acceptance") == both and resolve(client, "web.acceptance.svc") == both
        assert podman("exec", client, "nc", "web.acceptance.svc", "8080") in {"web-0", "web-1"}
        self.record("a service's names resolve to every replica of its target in the namespace and reach them")

        resources = [replica_set("web", 2, "b"), web_service, ordered_set("db", 2), service("db", headless=True)]
        revision = self.apply(resources)
        seen_first_running_when_second_appeared = None
        while True:
            status = self.status()
            db = self.pods(status, "db")
            if 1 in db and seen_first_running_when_second_appeared is None:
                seen_first_running_when_second_appeared = db[0]["state"] == "running"
            if status["converged"] and status["observed_revision"] == revision:
                break
            time.sleep(0.2)
        assert seen_first_running_when_second_appeared, "db-1 was created before db-0 was running"
        db = self.pods(status, "db")
        started = [podman("inspect", "--format", "{{.State.StartedAt}}", self.container(db[n])) for n in (0, 1)]
        zero, one = resolve(client, "db-0.db"), resolve(client, "db-1.db")
        assert zero and one and len(zero) == 1 and len(one) == 1 and zero != one, (zero, one)
        assert resolve(client, "db") == zero | one
        assert resolve(client, "db-1.db.acceptance.svc") == one
        assert podman("exec", client, "nc", "db-1.db", "8080") == "db-1"
        self.record(f"an ordered replica set with a headless service starts one replica at a time ({started}) "
                    "and each replica resolves as <name>-<N>.<service>")

        self.settled(self.apply([replica_set("probe", 1, "x")], deployment=self.deployment + "-x",
                                namespace="isolated"))
        status = self.status()
        outsider = self.container(self.pods(status, "probe", self.deployment + "-x")[0])
        assert resolve(outsider, "web") is None and resolve(outsider, "web.acceptance.svc") is None
        for address in both:
            reply = subprocess.run(["podman", "exec", outsider, "timeout", "5", "nc", address, "8080"],
                                   capture_output=True, text=True, timeout=30).stdout.strip()
            assert reply not in {"web-0", "web-1"}, f"another namespace reached {address} by IP"
        assert resolve(client, "probe") is None
        try:
            self.apply([replica_set("web", 1, "c")], deployment=self.deployment + "-c")
            raise AssertionError("a colliding deployment was accepted")
        except urllib.error.HTTPError as error:
            body = error.read().decode()
            assert error.code == 400 and "is also a workload name" in body, (error.code, body)
        self.record("namespaces are isolated (no names, no traffic by IP), and a deployment whose names collide in a shared namespace is refused")

        current = self.container_ids(self.status())
        revision = self.apply([replica_set("web", 2, "b"), service("web", port=80, target_port=8080),
                               ordered_set("db", 2), service("db", headless=True)])

        def unexecuted():
            s = self.status()
            return s if s["observed_revision"] == revision and s["unexecuted"] else None
        status = wait("the unexecuted report", unexecuted)
        assert "services whose port differs from the target port are not executed yet (rezn#123)" in \
            status["unexecuted"][0]["reasons"][0], status
        time.sleep(3)
        assert self.container_ids(self.status()) == current
        self.settled(self.apply(resources))
        self.record("a service that remaps its port is reported unexecuted and changes nothing")

    def cleanup(self):
        errors = []
        self.stop("rezn")
        if "orqos" in self.children:
            try:
                _, pods = http(self.orqos_url + "/workload/pods", token=self.token)
                for pod in pods:
                    if pod["spec"].get("labels", {}).get(OWNER) == self.owner or pod["id"] == self.foreign:
                        http(f"{self.orqos_url}/workload/pods/{pod['id']}", method="DELETE", token=self.token,
                             timeout=120)
            except Exception as error:  # cleanup must continue
                errors.append(f"removing pods: {error!r}")
            try:
                _, volumes = http(self.orqos_url + "/workload/volumes", token=self.token)
                for volume in volumes:
                    if volume["spec"].get("labels", {}).get(OWNER) == self.owner:
                        http(f"{self.orqos_url}/workload/volumes/{volume['name']}", method="DELETE",
                             token=self.token, timeout=120)
            except Exception as error:  # cleanup must continue
                errors.append(f"removing volumes: {error!r}")
            try:
                _, owned = http(self.orqos_url + "/workload/secrets", token=self.token)
                for secret in owned:
                    if secret.get("labels", {}).get(OWNER) == self.owner:
                        http(f"{self.orqos_url}/workload/secrets/{secret['name']}", method="DELETE",
                             token=self.token, timeout=120)
            except Exception as error:  # cleanup must continue
                errors.append(f"removing secrets: {error!r}")
        self.stop("orqos")
        if errors:
            raise AssertionError("cleanup failed: " + "; ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--orqos-bin", type=lambda p: Path(p).resolve(),
                        default=ROOT.parent / "orqos/target/debug/orqos")
    parser.add_argument("--rezn-bin", type=lambda p: Path(p).resolve(), default=ROOT / "target/debug/rezn")
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise SystemExit("run as root: Orqos's workload backend drives rootful Podman")
    with tempfile.TemporaryDirectory(prefix="rezn-acceptance-") as directory:
        acceptance = Acceptance(args, directory)
        try:
            steps = acceptance.execute()
        except BaseException:
            for name in ("orqos", "rezn"):
                log = Path(directory) / f"{name}.log"
                if log.exists():
                    print(f"--- {name} log (last 60 lines) ---\n" + "\n".join(log.read_text().splitlines()[-60:]))
            raise
        finally:
            acceptance.cleanup()
            stop_registry()
    print(json.dumps({"acceptance": "passed", "steps": steps}, indent=2))


if __name__ == "__main__":
    main()
