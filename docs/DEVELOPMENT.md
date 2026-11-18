# Development

Rezn accepts signed [IR v2](IR-v2.md) programs and reconciles them into systemd/Podman pods through
[Orqos](https://github.com/orqos-project/orqos)'s workload backend ([ADR 0001](adr/0001-execution-foundation.md)).
Use Rust 1.99.0 and the system OpenSSL development libraries (`build-essential`, `pkg-config`, `libssl-dev` on
Ubuntu).

## Checks

```bash
cargo fmt --check
cargo build --locked --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets
python3 -m unittest discover -s scripts -p 'test_spec.py'   # needs python3-jsonschema and python3-yaml
```

Push/PR CI runs these. The controller tests run against an in-memory mock of Orqos's workload API. The live
acceptance (below) runs against real systemd and Podman in the *Acceptance* workflow on Ubuntu 26.04.

Pre-existing warnings remain in the secret routes and store: an unused import, unused secret import/export methods
and `Option::map` returning unit. Cargo also reports future incompatibility in the existing `proc-macro-error2`
dependency.

## Run locally

Rezn needs Orqos with the workload backend: root, systemd 254 or newer, Podman 5 or newer and subordinate IDs for
the `containers` user (Orqos `docs/WORKLOAD_API.md`). Start Orqos from the sibling checkout with a random token:

```bash
export ORQOS_WORKLOAD_TOKEN="$(openssl rand -hex 32)"
sudo --preserve-env=ORQOS_WORKLOAD_TOKEN ORQOS_BACKENDS=workload BIND_ADDR=127.0.0.1:9090 ../orqos/target/debug/orqos
```

Then Rezn, with the same token:

```bash
STATE_DB_PATH=./rezn-local \
SECRETS_DB_PATH=./rezn-local-secrets \
ORQOS_API_URL=http://127.0.0.1:9090 \
ORQOS_WORKLOAD_TOKEN="$ORQOS_WORKLOAD_TOKEN" \
RECONCILE_INTERVAL=15 \
cargo run --locked -p rezn
```

`ORQOS_WORKLOAD_TOKEN` is required (at least 32 bytes); `ORQOS_API_URL` defaults to `http://127.0.0.1:9090`,
`BIND_ADDR` to `127.0.0.1:4000`. `RECONCILE_INTERVAL` must be positive seconds. Startup creates or validates
controller state before serving HTTP; a corrupt or unrecognized database stops startup. Within a running process,
state-read/validation failures stop reconciliation and return HTTP 500 from the state and status endpoints.

Sign and apply the runnable example, then watch it converge:

```bash
python3 scripts/signed_program.py examples/ir-v2/runnable.json > /tmp/demo.signed.json
cargo run --locked -p reznctl -- http://127.0.0.1:4000/apply demo /tmp/demo.signed.json
curl -s http://127.0.0.1:4000/status
sudo podman ps --filter label=dev.rezn.deployment=demo
```

`reznctl`'s first argument is the complete `/apply` URL. The signing helper handles programs with ASCII strings and
integers and generates a temporary key; general JSON signing needs full JCS canonicalization. Read the
[signature limits](SIGNATURES.md) before using signed programs outside local development. Validate a program
offline, bare or inside a signed envelope, with `cargo run --locked -p reznctl -- validate <file>`.

## Apply contract

`POST /apply` accepts `{"name": "demo", "instruction_wrapper": {"program": {...}, "signature": {"algorithm":
"ed25519", "pub": "base64 public key", "sig": "base64 signature"}}}`. The program is an IR v2 object: Rezn verifies
the signature over its JCS canonical form, validates it completely (the rules of `docs/IR-v2.md`, reported as
`JSON-pointer: message` in the HTTP 400 body), requires its `deployment` to equal `name` and stores the envelope
intact.

**IR v1 is retired.** Instruction arrays are refused with HTTP 400, and a state database that still holds an IR v1
deployment is refused at startup: use a new database (IR v1 Docker containers are left untouched; remove them
yourself). The Rezn DSL still emits IR v1, so its output is not executable until it emits IR v2
([rezn-dsl#29](https://github.com/rezn-project/rezn-dsl/issues/29)).

IR v2 lets deployments share a namespace only if their resource names do not collide (workload kinds share one
name class; every other kind is its own). `/apply` enforces this against the other stored deployments: a colliding
program gets HTTP 400 with `JSON-pointer: "name" is also a <class> name in deployment "<other>" of namespace
"<ns>"`.

Invalid signatures or programs return HTTP 400 with the reason. Malformed JSON and unknown envelope keys return an
extractor 4xx (normally 422). Invalid submissions write no state and trigger no change. Success is `202
{"stored":true,"revision":N}`: durable stored intent, not completed deployment. Each accepted apply increments a
global revision. Apply replaces only the named deployment; a program with `"resources": []` removes its pods while
keeping the deployment record.

## What runs

Orqos's workload backend runs pods of plain containers (image, `command`, `args`, literal environment values,
restart policy and grace period) on owned networks with DNS aliases. A deployment runs only when **every** resource
maps exactly onto that:

- `replica_set` and `ordered_replica_set` resources (the latter without `volume_claims`), each replica as one Orqos
  pod: one Podman pod with its own network namespace, user namespace and hostname `<name>-<ordinal>`;
- `service` resources whose every `port` equals its target port (a number, or the named container port it
  resolves to), as DNS names (below);
- `config` resources, which are inert until something consumes them;
- containers with `pull` absent or `if_not_present`, environment `value`s, and declared `ports` (informational, as in
  Kubernetes: nothing is published on the host).

Anything else leaves the whole deployment **unexecuted**: services that remap ports
([#123](https://github.com/rezn-project/rezn/issues/123)), ingress, network policies, secrets, jobs, volume claims,
phases, init containers and sidecars, pod and container security settings, volumes and mounts, probes, resource
requests and limits, other pull policies, registry credentials, and secret or pod-IP environment references.
`/status` lists every blocking field as `JSON-pointer: reason (issue)`, citing the issue that adds it. An
unexecuted deployment's pods from an earlier revision keep running untouched.

### Networking and services

Every pod of a namespace joins that namespace's network: an Orqos-owned Podman bridge network with aardvark-dns,
named `orqos-net-rz-<first 24 hex digits of sha256([owner, "namespace", namespace])>` and shared by this owner's
deployments in the namespace. Orqos creates it with the first pod and removes it after the last, strictly
isolated (netavark `isolate=strict`), so pods of different namespaces can neither resolve nor reach each other,
even by IP, while egress to the outside keeps working (stricter than Kubernetes'
flat pod network, as IR v2 scopes names to the namespace).

A service becomes DNS aliases on every pod of its target:

- `<service>`, `<service>.<namespace>` and `<service>.<namespace>.svc` resolve to the addresses of all the target's
  replicas;
- a headless service also gives each replica `<name>-<N>.<service>`, `<name>-<N>.<service>.<namespace>` and
  `<name>-<N>.<service>.<namespace>.svc`.

Clients connect to the container's own port, so a service whose `port` differs from its target port is not
executed yet. A non-headless service resolves to every replica's address (DNS round-robin) rather than one virtual
address ([#123](https://github.com/rezn-project/rezn/issues/123)). Names resolve only while a replica runs; without
probes (not executed yet) a running replica is a ready one, which is IR v2's membership rule. Aliases are part of
the pod spec: adding, removing or changing a service replaces its target's pods through the rolling replacement
below. Changing membership without a restart arrives with readiness
([#59](https://github.com/rezn-project/rezn/issues/59)).

## Reconciliation

One serialized loop runs after every apply and every `RECONCILE_INTERVAL`. Each pass:

1. loads and validates the whole stored state, translating each deployment into desired pods (deployments whose
   names collide in a shared namespace, which only state stored before `/apply` checked this can hold, stay
   unexecuted);
2. lists Orqos's pods and keeps those whose labels claim this database's owner, proving each claim (below);
3. plans and applies a bounded set of changes, per replica set:
   - removes pods of removed resources or deployments, and replicas beyond the count (highest ordinals);
   - creates missing replicas;
   - replaces pods whose spec changed: those not running at once, a running one (highest ordinal first) only
     while every other replica of the set exists and runs, so a rolling change keeps the rest serving;
   - re-applies an unchanged pod Orqos reports `stopped` (for example after a host reboot), which starts it;
   - leaves `restarting`, `degraded` and `failed` pods to systemd's restart backoff, and reports them.
   Ordered replica sets with `ordered` management create the lowest missing replica only once all lower ones run,
   and remove the highest only once the others run.
4. observes again and succeeds only when the executed deployments match.

Every planned change is attempted even if an earlier one fails; failures keep Orqos's operation, HTTP status and
message and appear in `/status` `errors` until a later pass succeeds. If an apply lands during a pass, the pass
retries with the new revision.

## Observable status

`GET /state` and `GET /state/raw` return the accepted IR v2 program objects by deployment. Corrupt, missing or
invalid persisted state returns HTTP 500, never an empty map. `GET /status` reports:

| Field | Meaning |
| --- | --- |
| `owner` | Persistent database ownership identity |
| `desired_revision`, `observed_revision` | Latest stored revision and the revision of the last successful observation |
| `last_observation`, `last_attempt` | UTC RFC3339 times; observation is null until a listing succeeds |
| `observation` | `unknown` before any successful listing; `stale` after a failed pass; `pending` when the observation belongs to an older revision; otherwise `fresh` |
| `converged` | True only when the observation is fresh for this revision, the last pass had no errors, no deployment is unexecuted, every desired pod runs with its exact spec and no other owned pod exists |
| `errors` | The last pass's validation, observation, mutation or convergence errors; cleared after a successful pass |
| `unexecuted[]` | `deployment` and `reasons` (`JSON-pointer: reason (issue)`) for each program the backend cannot run yet |
| `workloads[]` | Per replica set: `deployment`, `kind`, `resource`, `desired_replicas` (null for pods awaiting removal or of unexecuted deployments), `running_replicas` (null until observed) and `pods[]` |
| `pods[]` | `id`, `ordinal`, `hostname`, Orqos's `state` (`running`, `starting`, `restarting`, `degraded`, `failed`, `stopped`), `current` (runs the desired spec), `aliases` (its DNS names on the namespace network), `containers[]` (`name`, `active_state`, `sub_state`, `result`, `restarts`) and Orqos's last `error` |

After a failure, status keeps the last successful observation with its timestamp and marks it stale; it never
invents zero replicas or declares success. It starts `unknown` after a restart and rebuilds from Orqos's listing.

OpenAPI and Swagger are at `/api/openapi.json` and `/swagger`.

## State format and ownership

The state database stores one JSON value at `controller/v1`: `format`, a random 256-bit `owner`, a monotonic
`revision`, and deployments with their original signed envelope and metadata (`sig_id`, `applied_at`, resource
kinds and names). One atomic sled insertion stores intent and metadata, followed by a durable flush before HTTP
acceptance. Rezn validates the whole record, signatures and programs before any change.

New databases are **format 2**. Format 1 held IR v1 programs: an empty format 1 database still loads and becomes
format 2 on the next apply; one with deployments is refused. Databases with the older `desired`/`instruction/*`
keys are refused too; there is no migration or adoption. Copying a state database to another controller duplicates
its ownership identity, so run one process per identity. Losing the database leaves its pods unproven and
untouched by a new controller.

Each replica is one Orqos pod with ID `rz-<first 24 hex digits of sha256([owner, deployment, kind, resource,
ordinal])>` and these labels on the Podman pod and its containers:

```text
dev.rezn.owner=<64-character hex owner identity>
dev.rezn.deployment=<deployment>
dev.rezn.namespace=<namespace>
dev.rezn.kind=replica_set | ordered_replica_set
dev.rezn.resource=<resource name>
dev.rezn.ordinal=<replica number>
```

Rezn checks labels locally: a listing is not proof. Pods of other owners, or without the owner label, are
ignored. A pod that claims this owner but has missing or malformed labels, or an ID that differs from the one its
labels produce, fails the whole observation before any change. Orqos separately refuses to touch units and Podman
pods it does not own (its own `dev.orqos.owner` token).

## Live acceptance

From this repository, with Orqos built in the sibling checkout and the prerequisites above:

```bash
cargo build --locked --workspace
sudo python3 scripts/acceptance.py --orqos-bin ../orqos/target/debug/orqos --rezn-bin target/debug/rezn
```

The script starts its own Orqos (workload backend) and Rezn on loopback ports with temporary state and keys,
applies signed IR v2 programs and checks with Podman itself:

- a replica set runs one pod per replica with the right hostnames and environment;
- an unchanged re-apply keeps every container;
- scaling 2 → 3 → 1 touches only the highest ordinals;
- a changed spec replaces replicas one at a time, highest first, never stopping both;
- a program needing unsupported features is reported with reasons and changes nothing;
- a restarted Rezn keeps its identity and changes nothing;
- a killed container is restarted by systemd without Rezn replacing the pod;
- a service's names resolve inside the namespace to every replica of its target, and connections by name arrive;
- an ordered replica set with a headless service starts its replicas one at a time, and each resolves as
  `<name>-<N>.<service>`;
- a pod of another namespace cannot resolve those names, and a deployment whose names collide in a shared namespace
  is refused;
- a service that remaps its port is reported unexecuted and changes nothing;
- empty programs remove every owned pod and then the namespace networks, while a foreign Orqos pod stays.

It removes everything it created, including after a failure. The *Acceptance* workflow runs it on Ubuntu 26.04
against the Orqos commit pinned in `.github/workflows/acceptance.yml`.
