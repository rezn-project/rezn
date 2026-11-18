#!/usr/bin/env python3
"""Live acceptance: signed IR v2 programs become systemd/Podman pods through Orqos's workload backend.

Requires root, systemd >= 254, Podman >= 5 and subordinate ID ranges for the `containers` user (see Orqos
docs/WORKLOAD_API.md). Starts its own Orqos (ORQOS_BACKENDS=workload) and Rezn on loopback ports with temporary
state, and removes everything it created, including after a failure.

    sudo python3 scripts/acceptance.py --orqos-bin ../orqos/target/debug/orqos --rezn-bin target/debug/rezn
"""
import argparse
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
        except (OSError, ValueError, AssertionError, KeyError) as error:
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
        assert set(changed_at) == {0, 1} and changed_at[1] < changed_at[0], changed_at
        for pod in self.pods(status).values():
            assert podman("exec", self.container(pod), "printenv", "LEVEL") == "b"
        self.record("a changed spec replaces replicas one at a time, highest ordinal first")

        current = self.container_ids(status)
        probed = replica_set("web", 2, "c")
        probed["fields"]["pod"]["containers"][0]["readiness"] = {"exec": ["true"]}
        revision = self.apply([probed])

        def unexecuted():
            s = self.status()
            return s if s["observed_revision"] == revision and s["unexecuted"] else None
        status = wait("the unexecuted report", unexecuted)
        assert not status["converged"] and status["unexecuted"][0]["deployment"] == self.deployment
        assert "readiness: probes are not executed yet" in status["unexecuted"][0]["reasons"][0], status
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

        def run(pod, script):
            return podman("exec", self.container(pod), "sh", "-c", script)

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
        for pod in pods.values():
            out = run(pod, "ls /etc/api | tr '\\n' ' '; echo; cat /etc/api/conf/app.json; echo; cat /opt/app.json; echo; "
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
    print(json.dumps({"acceptance": "passed", "steps": steps}, indent=2))


if __name__ == "__main__":
    main()
