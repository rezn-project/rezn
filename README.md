# Rezn

Rezn is a declarative workload controller aiming to replace Kubernetes on a single Linux host. It accepts signed
[IR v2](docs/IR-v2.md) programs and reconciles them into systemd/Podman pods through
[Orqos](https://github.com/orqos-project/orqos)'s workload backend ([ADR 0001](docs/adr/0001-execution-foundation.md)).
See the [roadmap](https://github.com/rezn-project/rezn/issues/25).

What runs today: `replica_set` and `ordered_replica_set` resources (image, command, arguments, literal environment
values, user and groups, read-only root, no privilege escalation, no capabilities, grace period), `service`
resources as DNS names, `volume_claim`s and per-replica claims with retention, scratch volumes, and `config` and
`secret` resources as read-only files and secret environment variables. Secret values come from Rezn's encrypted
secret store (set per namespace by an operator, or generated: random values, certificate authorities and
certificates for service names, renewed before expiry), never from the program, and a changed config or
secret value replaces exactly the pods that use it. Volumes are mounted idmapped, so data keeps its container IDs across pods and restarts. Each replica is
one Podman pod with its own network and user namespace, supervised by systemd with restart backoff, on its
namespace's network, where `<service>`, `<service>.<namespace>.svc` and, for headless services,
`<name>-<N>.<service>` resolve. Scaling touches the highest ordinals, ordered sets start one replica at a time,
changed specs roll one replica at a time, and stopped pods are started again. Pods may have init containers (run once, in order) and sidecars (started first; a sidecar's startup probe gates the main containers). Images are pulled with the pod's registry credentials and pull policy. Resource limits become cgroup limits, and a revision whose requests do not fit the host is refused. Orqos runs startup, readiness and liveness probes (exec, and HTTP(S) from inside the pod), and rollouts, ordered starts and convergence wait for readiness. A program that needs anything else
(port-remapping services, jobs, ingress, network policies and so on) is stored but left **unexecuted**, and `/status` names every blocking field with the issue that adds it. Nothing
is silently dropped. IR v1 programs are refused; the [Kubernetes manifest subset](docs/KUBERNETES-SUBSET.md) is
specified for import.

```bash
python3 scripts/signed_program.py examples/ir-v2/runnable.json > /tmp/demo.signed.json
cargo run --locked -p reznctl -- http://127.0.0.1:4000/apply demo /tmp/demo.signed.json
curl -s http://127.0.0.1:4000/status
```

`/apply` returns HTTP 202 when intent is stored, before deployment completes. `/state` reports the accepted
programs; `/status` reports observation freshness, convergence, errors, unexecuted deployments with reasons, and
every pod with its state, container restarts and whether it runs the current spec. Reconciliation runs after each
apply and periodically.

Each state database has a persistent ownership identity, carried in labels on every pod Rezn creates and proven
from them before any change; Rezn never adopts pods it cannot prove. See the
[development guide](docs/DEVELOPMENT.md) for running Orqos and Rezn, the apply and status contracts, the state
format and the live acceptance against real systemd and Podman.

Signatures verify canonical program integrity against the **caller-supplied public key**. They do not establish
signer trust or prevent replay; see [signature limits](docs/SIGNATURES.md). RawPair's native workspaces use Orqos's
host API, not Rezn ([scope](docs/HOST_WORKSPACES.md)).
