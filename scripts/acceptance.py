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


def replica_set(name, replicas, level):
    return {"kind": "replica_set", "name": name, "fields": {"replicas": replicas, "pod": {
        "grace_seconds": 5,
        "containers": [{"name": "app", "image": IMAGE, "pull": "if_not_present",
                        "command": ["/bin/sh", "-c", "trap 'exit 0' TERM; while true; do sleep 1; done"],
                        "env": [{"name": "LEVEL", "value": level}]}]}}}


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

    def apply(self, resources):
        program = {"ir": "rezn.dev/v2", "deployment": self.deployment, "namespace": "acceptance",
                   "resources": resources}
        code, accepted = http(self.rezn_url + "/apply", {
            "name": self.deployment, "instruction_wrapper": sign(program, self.key)})
        assert code == 202 and accepted["stored"] is True, accepted
        return accepted["revision"]

    def status(self):
        return http(self.rezn_url + "/status")[1]

    def settled(self, revision):
        def check():
            status = self.status()
            return status if (status["converged"] and status["observation"] == "fresh"
                              and status["desired_revision"] == revision
                              and status["observed_revision"] == revision) else None
        return wait(f"convergence of revision {revision}", check)

    def pods(self, status, resource="web"):
        return {p["ordinal"]: p for w in status["workloads"]
                if w["deployment"] == self.deployment and w["resource"] == resource for p in w["pods"]}

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

        status = self.settled(self.apply([]))
        assert not self.pods(status)
        wait("owned Podman pods to disappear", lambda: not self.owned_podman_pods(), 60)
        self.foreign_intact()
        self.record("an empty program removes every owned pod; a foreign Orqos pod is untouched")
        return self.steps

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
