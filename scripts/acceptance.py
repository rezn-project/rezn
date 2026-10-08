#!/usr/bin/env python3
"""Live signed HTTP lifecycle acceptance. Owns only its child processes/fixtures."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from signed_program import generate_key, sign

ROOT = Path(__file__).resolve().parents[1]


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def http(url, body=None, timeout=3):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        raw = response.read()
        return response.status, json.loads(raw) if raw else None


def wait(description, check, timeout=60):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, AssertionError) as error:
            last = str(error)
        time.sleep(0.2)
    raise AssertionError(f"Timed out waiting for {description}; last error: {last}")


def pod(name="web", image="nginx:alpine", replicas=1, ports=(80,)):
    return {"kind": "pod", "name": name,
            "fields": {"image": image, "replicas": replicas, "ports": list(ports)}}


class Acceptance:
    def __init__(self, args, directory):
        self.args = args
        self.directory = Path(directory)
        self.run = "rezn-accept-" + uuid.uuid4().hex
        self.key = generate_key(directory)
        self.orqos_url = f"http://127.0.0.1:{free_port()}"
        self.rezn_url = f"http://127.0.0.1:{free_port()}"
        self.children = {}
        self.logs = []
        self.owner = None
        self.foreign = None
        self.steps = []

    def docker(self, *args):
        result = subprocess.run(["docker", "--context", self.args.docker_context, *args],
                                check=True, capture_output=True, text=True, timeout=30)
        return result.stdout.strip()

    def start(self, name, binary, env):
        assert name not in self.children, f"{name} is still registered"
        log = self.directory / f"{name}-{len(self.logs)}.log"
        self.logs.append(log)
        with log.open("wb") as output:
            process = subprocess.Popen([str(binary)], cwd=self.directory,
                                       env={**os.environ, **env}, stdout=output, stderr=output)
        self.children[name] = process
        url = self.rezn_url + "/state" if name == "rezn" else self.orqos_url + "/docker/containers?all=true"

        def ready():
            assert process.poll() is None, f"{name} exited: {log.read_text()}"
            return http(url)[0] == 200
        wait(f"{name} startup", ready, 20)

    def stop(self, name):
        process = self.children.get(name)
        if process is None:
            return
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        # The handle is terminal before any restart.
        del self.children[name]

    def start_orqos(self):
        endpoint = self.docker("context", "inspect", self.args.docker_context,
                               "--format", "{{.Endpoints.docker.Host}}")
        self.start("orqos", self.args.orqos_bin, {"ORQOS_BACKENDS": "docker",
                   "DOCKER_SOCKET": endpoint, "BIND_ADDR": self.orqos_url.removeprefix("http://")})

    def start_rezn(self):
        self.start("rezn", self.args.rezn_bin, {
            "STATE_DB_PATH": str(self.directory / "state"),
            "SECRETS_DB_PATH": str(self.directory / "secrets"),
            "REZN_AGE_IDENTITY": str(self.directory / "identity.txt"),
            "ORQOS_API_URL": self.orqos_url, "STATS_WS_URL": self.orqos_url.replace("http://", "ws://") + "/stats/ws",
            "BIND_ADDR": self.rezn_url.removeprefix("http://"), "RECONCILE_INTERVAL": "1"})
        status = http(self.rezn_url + "/status")[1]
        if self.owner is not None:
            assert status["owner"] == self.owner, "restart changed ownership"
        self.owner = status["owner"]

    def apply(self, program):
        code, accepted = http(self.rezn_url + "/apply", {
            "name": self.run, "instruction_wrapper": sign(program, self.key)})
        assert code == 202 and accepted["stored"] is True
        return accepted["revision"]

    def owned_ids(self):
        return sorted(self.docker("ps", "-aq", "--no-trunc", "--filter", f"label=dev.rezn.owner={self.owner}").split())

    def assert_foreign(self):
        fixture = json.loads(self.docker("inspect", self.foreign))[0]
        assert fixture["State"]["Running"] and fixture["Id"] == self.foreign
        assert fixture["Config"]["Labels"]["dev.rezn.acceptance"] == self.run

    def settled(self, revision, program):
        desired = {p["name"]: p["fields"] for p in program}
        def check():
            status = http(self.rezn_url + "/status")[1]
            if not (status["converged"] and status["desired_revision"] == revision
                    and status["observed_revision"] == revision and status["observation"] == "fresh"):
                return False
            workloads = {w["pod"]: w for w in status["workloads"] if w["deployment"] == self.run}
            assert set(workloads) == set(desired), status
            for name, fields in desired.items():
                w = workloads[name]
                assert w["desired"] == fields and w["desired_replicas"] == fields["replicas"]
                assert w["running_replicas"] == fields["replicas"]
                assert len(w["containers"]) == fields["replicas"]
                for c in w["containers"]:
                    assert c["State"] == "running" and c["Image"] == fields["image"]
                    assert c["Labels"]["dev.rezn.configuration"] == w["desired_configuration"]
                    for port in fields["ports"]:
                        assert any(p["PrivatePort"] == port and p["Type"] == "tcp" and p.get("PublicPort", 0) > 0 for p in c["Ports"])
            return status
        status = wait(f"revision {revision} convergence", check)
        status_ids = sorted(c["Id"] for w in status["workloads"] for c in w["containers"])
        assert status_ids == self.owned_ids(), "stopped or unreported owned leftovers"
        # Compare observed mappings against Docker itself, independently of Rezn.
        for w in status["workloads"]:
            for c in w["containers"]:
                inspected = json.loads(self.docker("inspect", c["Id"]))[0]
                assert inspected["State"]["Running"]
                for p in c["Ports"]:
                    if p.get("PublicPort"):
                        bindings = inspected["NetworkSettings"]["Ports"][f'{p["PrivatePort"]}/{p["Type"]}']
                        assert any(int(b["HostPort"]) == p["PublicPort"] and b["HostIp"] == p["IP"] for b in bindings)
        self.assert_foreign()
        return status

    @staticmethod
    def ids(status, name="web"):
        return sorted(c["Id"] for w in status["workloads"] if w["pod"] == name for c in w["containers"])

    def serves_http(self, status):
        mappings = [p for w in status["workloads"] if w["pod"] == "web" for c in w["containers"] for p in c["Ports"]
                    if p["PrivatePort"] == 80 and p["Type"] == "tcp" and p.get("PublicPort", 0) > 0 and p.get("IP") == "0.0.0.0"]
        assert mappings, "no nonzero observed HTTP mapping"
        port = mappings[0]["PublicPort"]
        def responds():
            with urllib.request.urlopen(f"http://127.0.0.1:{port}", timeout=3) as response:
                return response.status == 200 and bool(response.read())
        wait(f"HTTP on observed host port {port}", responds, 20)
        return port

    def record(self, description):
        self.steps.append(description)
        print(f"PASS {description}", flush=True)

    def execute(self):
        self.docker("info", "--format", "{{.ServerVersion}}")
        for image in ("nginx:alpine", "httpd:alpine"):
            self.docker("image", "inspect", image)  # Never pull implicitly.
        self.start_orqos()
        self.start_rezn()
        self.foreign = self.docker("run", "-d", "--pull=never", "--name", self.run + "-web-foreign",
                                   "--label", f"dev.rezn.acceptance={self.run}",
                                   "--label", f"mol={self.run}", "--label", f"pod={self.run}:web", "nginx:alpine")
        program = [pod()]
        revision = self.apply(program)
        first = self.settled(revision, program)
        port = self.serves_http(first)
        self.record(f"signed HTTP apply creates a replica; observed host port {port} serves HTTP")
        revision = self.apply(program)
        repeated = self.settled(revision, program)
        assert self.ids(first) == self.ids(repeated)
        self.record("unchanged reapply preserves container identities")
        before = http(self.rezn_url + "/state")[1]
        invalid = [pod()]
        invalid[0]["fields"]["env"] = {"IGNORED": "must-be-rejected"}
        try:
            self.apply(invalid)
            raise AssertionError("unsupported signed program was accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 400 and b"unknown field" in error.read()
        assert http(self.rezn_url + "/state")[1] == before
        assert self.owned_ids() == self.ids(repeated)
        self.record("unsupported signed intent changes neither state nor containers")
        for replicas in (2, 1, 0):
            program = [pod(replicas=replicas)]
            revision = self.apply(program)
            current = self.settled(revision, program)
            if replicas == 2:
                assert set(self.ids(first)).issubset(self.ids(current))
            self.record(f"scale to {replicas} without stopped leftovers")
        program = [pod()]
        current = self.settled(self.apply(program), program)
        program = [pod(image="httpd:alpine")]
        changed = self.settled(self.apply(program), program)
        assert self.ids(current) != self.ids(changed)
        self.serves_http(changed)
        self.record("image change replaces containers at equal replica count")
        program = [pod(image="httpd:alpine", ports=(80, 81))]
        changed_ports = self.settled(self.apply(program), program)
        assert self.ids(changed) != self.ids(changed_ports)
        self.record("port change replaces containers at equal replica count")
        program = [*program, pod(name="side", ports=())]
        two = self.settled(self.apply(program), program)
        program = [program[0]]
        removed = self.settled(self.apply(program), program)
        assert self.ids(two) == self.ids(removed)
        assert not self.ids(removed, "side")
        self.record("remove one pod while preserving the remaining workload")
        revision = removed["desired_revision"]
        self.stop("rezn")
        self.start_rezn()
        restarted = self.settled(revision, program)
        assert self.ids(removed) == self.ids(restarted)
        self.serves_http(restarted)
        self.record("real process restart preserves ownership, IDs and observed port mappings")
        self.docker("stop", self.ids(restarted)[0])
        wait("stopped replica replacement", lambda: self.ids(http(self.rezn_url + "/status")[1]) != self.ids(restarted))
        recovered = self.settled(revision, program)
        assert self.ids(recovered) != self.ids(restarted)
        self.record("stopped replica is replaced with no debris")
        self.stop("orqos")
        wait("stale backend status", lambda: http(self.rezn_url + "/status")[1]["observation"] == "stale", 30)
        stale = http(self.rezn_url + "/status")[1]
        assert not stale["converged"] and stale["errors"]
        assert stale["last_observation"] and stale["workloads"][0]["running_replicas"] == 1
        retained = self.owned_ids()
        program = [pod(image="nginx:alpine", replicas=2)]
        revision = self.apply(program)
        assert self.owned_ids() == retained
        self.assert_foreign()
        self.start_orqos()
        recovered = self.settled(revision, program)
        self.serves_http(recovered)
        self.record("backend failure reports stale observations; stored updates converge after recovery")
        revision = self.apply([])
        self.settled(revision, [])
        self.record("signed empty program removes all owned workloads and leaves foreign container untouched")
        self.stop("rezn")
        self.start_rezn()
        self.settled(revision, [])
        self.record("restart after empty intent does not resurrect workloads")
        return {"docker_context": self.args.docker_context, "steps": self.steps}

    def cleanup(self):
        errors = []
        for name in list(self.children):
            try:
                self.stop(name)
            except Exception as error:
                errors.append(f"stop {name}: {error}")
        if self.owner:
            try:
                for container_id in self.owned_ids():
                    container = json.loads(self.docker("inspect", container_id))[0]
                    labels = container["Config"]["Labels"]
                    assert labels.get("dev.rezn.owner") == self.owner and labels.get("dev.rezn.managed") == "v1"
                    self.docker("rm", "-f", container_id)
                assert not self.owned_ids()
            except Exception as error:
                errors.append(f"owned fixtures: {error}")
        # The run marker also handles a fixture created before the response was lost.
        try:
            ids = self.docker("ps", "-aq", "--no-trunc", "--filter", f"label=dev.rezn.acceptance={self.run}").split()
            for container_id in ids:
                container = json.loads(self.docker("inspect", container_id))[0]
                assert container["Config"]["Labels"].get("dev.rezn.acceptance") == self.run
                self.docker("rm", "-f", container_id)
        except Exception as error:
            errors.append(f"foreign fixture: {error}")
        if errors:
            raise AssertionError("cleanup failed: " + "; ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker-context", required=True, help="explicit local Docker context")
    parser.add_argument("--orqos-bin", type=lambda p: Path(p).resolve(), default=ROOT.parent / "orqos/target/debug/orqos")
    parser.add_argument("--rezn-bin", type=lambda p: Path(p).resolve(), default=ROOT / "target/debug/rezn")
    args = parser.parse_args()
    for binary in (args.orqos_bin, args.rezn_bin):
        assert binary.is_file(), f"Build prerequisite binary first: {binary}"
    with tempfile.TemporaryDirectory(prefix="rezn-acceptance-") as directory:
        acceptance = Acceptance(args, directory)
        try:
            report = acceptance.execute()
        except BaseException:
            for log in acceptance.logs:
                print(f"\n{log.name}:\n{log.read_text()[-8000:]}", flush=True)
            raise
        finally:
            acceptance.cleanup()
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
