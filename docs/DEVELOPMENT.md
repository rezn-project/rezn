# Development

Rezn controls local Docker containers through Orqos. Use Rust 1.99.0 and the system OpenSSL development libraries (`build-essential`, `pkg-config`, `libssl-dev` on Ubuntu).

## Checks

```bash
cargo fmt --check
cargo build --locked --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets
```

Push/PR CI runs these ordinary checks without Docker. The live acceptance script is explicit and requires Python 3, OpenSSL 3 with Ed25519 support, a reachable local Docker engine/context, Docker-only Orqos, and pre-pulled test images. Dependencies and the lockfile are unchanged.

Pre-existing warnings remain in the secret routes/store and stats: an unused import, unused secret import/export methods, `Option::map` returning unit, and a needless borrow. Cargo also reports future incompatibility in the existing `proc-macro-error2` dependency. The new controller code adds no warnings.

## Run locally

With Orqos already built in the sibling checkout, start it using the intended Docker context's socket:

```bash
ORQOS_BACKENDS=docker \
DOCKER_SOCKET="$(docker context inspect desktop-linux --format '{{.Endpoints.docker.Host}}')" \
../orqos/target/debug/orqos
```

Select a **new** state database when upgrading from the previous format:

```bash
STATE_DB_PATH=./rezn-local-v1 \
SECRETS_DB_PATH=./rezn-local-secrets \
ORQOS_API_URL=http://127.0.0.1:3000 \
STATS_WS_URL=ws://127.0.0.1:3000/stats/ws \
RECONCILE_INTERVAL=15 \
cargo run --locked -p rezn
```

`BIND_ADDR` defaults to `127.0.0.1:4000`. `RECONCILE_INTERVAL` must be positive seconds. Startup creates or validates controller state before serving HTTP; a corrupt or unrecognized database stops startup. Within a running process, state-read/validation failures stop reconciliation and return HTTP 500 from the state/status endpoints.

Pull desired images explicitly into the same engine before applying them:

```bash
docker --context desktop-linux pull nginx:alpine
cargo run --locked -p reznctl -- http://127.0.0.1:4000/apply demo examples/test.ir.json
```

The actual binary is `reznctl`; its first argument is the complete `/apply` URL. It prints the accepted response and includes response details on HTTP failure. `examples/test.ir.json` is a newly signed supported program, not a stripped version of an unsupported signed bundle. `examples/web.rezn` illustrates the matching DSL subset. To produce a fresh development envelope without installing the DSL:

```bash
python3 scripts/signed_program.py examples/web.program.json > /tmp/web.signed.json
cargo run --locked -p reznctl -- http://127.0.0.1:4000/apply demo /tmp/web.signed.json
```

That signing helper supports this ASCII/integer subset and generates a temporary key. General JSON signing requires full JCS canonicalization. Read the [signature limits and DSL discrepancy](SIGNATURES.md) before using signed programs outside local development.

## Executable intent and apply contract

`POST /apply` accepts `{ "name": "demo", "instruction_wrapper": { "program": [...], "signature": { "algorithm": "ed25519", "pub": "base64 public key", "sig": "base64 signature" } } }`.

The signature covers JCS-canonicalized **submitted program values**, before typed validation. The original signed envelope is stored intact. Deployment and pod names must be 1–63 ASCII letters/digits, `-` or `_`, starting with a letter/digit. Pod names must be unique within a deployment; the same pod name in different deployments is independent.

Every instruction must have exactly `kind`, `name`, and `fields`. `kind` must be `pod`; fields must contain exactly `image`, `replicas` and `ports`. Unknown keys, any `options`, `env` or `secure` fields (including null/false), and service/volume/enum/secret instructions are rejected. Replicas must be nonnegative integers. Ports must be unique TCP container-port integers from 1 to 65535; `[]` publishes none. Host ports are dynamically allocated, and Orqos currently binds them on `0.0.0.0`.

Images use a bounded Docker-reference subset: lowercase repository components, optional DNS/IPv4/localhost registry and numeric port, optional Docker tag, and optional `@sha256:<64 lowercase hex digits>`. Empty, whitespace-containing, URL-like and malformed references are rejected. IPv6 registry syntax and other digest algorithms are outside this subset. A syntactically valid but missing image is accepted as intent and appears as a backend convergence error; Rezn never pulls it.

Invalid signatures or executable intent return HTTP 400 with the reason. Malformed JSON/envelopes and unknown envelope keys return an extractor 4xx response (normally 422; malformed JSON is 400). Invalid submissions write no state and trigger no container mutation. Successful responses are `202 {"stored":true,"revision":N}`: they acknowledge durable stored intent, not completed deployment. Each accepted apply increments a global database revision. Apply replaces only the named deployment; a signed `program: []` removes all its workloads while keeping the deployment record. `replicas: 0` keeps a desired pod with no containers.

## Observable status

`GET /state` and existing `GET /state/raw` return accepted desired instructions keyed by deployment. Corrupt, missing or invalid persisted state returns HTTP 500, never an empty map. `GET /status` reports:

| Field | Meaning |
| --- | --- |
| `owner` | Persistent database ownership identity |
| `desired_revision`, `observed_revision` | Latest stored revision and the snapshot used by the last successful owner-wide listing |
| `last_observation`, `last_attempt` | UTC RFC3339 times; observation is null until a listing succeeds |
| `observation` | `unknown` before any successful listing; `stale` after a failed pass; `pending` when observation belongs to an older revision; otherwise `fresh` |
| `converged` | True only when the observed revision equals desired, every owned container matches desired configuration and is running, all replica counts match, and the pass has no errors |
| `errors` | Last pass's validation, observation, mutation or convergence errors; cleared after a successful pass |
| `workloads[]` | Deployment/pod, desired fields and configuration hash, current configuration hashes, desired/running replica counts and observed containers |

Container entries use Docker listing names: `Id`, `Image`, `State`, `Labels`, `Ports`. Current configuration hashes are in `current_configurations` and the container's `dev.rezn.configuration` label; actual images and port mappings are in its `Image` and `Ports`. Each port mapping has `IP`, `PrivatePort`, `PublicPort`, `Type`. Unpublished ports have null `PublicPort`. Read allocated host ports from these observations: the create response's requested `host: 0` is not an allocated port.

Running counts are null when observations are unknown. After a failure, the API retains the last successfully observed containers/counts with their timestamp and marks them stale; it never invents zero replicas or declares success. Stale counts for newly desired pods come from the previous complete owner-wide listing and describe that older snapshot. Status is a snapshot, not a live Docker query on each GET. It starts unknown after restart, then reconstructs containers and ports from listings. Removed pods can remain visible with desired count zero while their observed containers await deletion. A newer apply immediately prevents an older observation from claiming convergence of that revision, even when the instructions are identical.

OpenAPI and Swagger are available at `/api/openapi.json` and `/swagger`.

## State format and ownership

The state database now stores one JSON value at `controller/v1`: `format: 1`, a random 256-bit `owner`, a monotonic `revision`, and deployments containing their original signed envelope and instruction metadata (`sig_id`, `applied_at`, instruction names/kinds). One atomic sled insertion stores intent and metadata together, followed by a durable flush before HTTP acceptance. Rezn validates the entire record, signatures and executable fields before any reconciliation mutation.

There is no automatic legacy migration or implicit adoption. Databases using the old `desired`/`instruction/*` keys without the new record are refused; select a fresh database explicitly and handle old containers manually. Copying the same state database to another controller duplicates its ownership identity, so run only one process against that identity. A restart with the original database preserves ownership; losing the database leaves those resources unproven and untouched by a new controller.

Owned containers carry all five labels:

```text
dev.rezn.owner=<persistent 64-character hex identity>
dev.rezn.managed=v1
dev.rezn.deployment=<deployment name>
dev.rezn.pod=<pod name>
dev.rezn.configuration=<sha256 of canonical image + sorted TCP ports>
```

Replica count is excluded from the configuration hash; port order is irrelevant. Container names are unique opaque `rezn-<random hex>` values and provide no ownership proof. The runtime requests `GET /docker/containers?all=true&label=dev.rezn.owner=...` and checks labels locally before ID-based mutations. Foreign identities and resources without the `v1` marker are untouched. Malformed proofs on resources claiming this identity and marker fail the whole observation before mutation.

One serialized loop observes all owned running **and stopped** containers, including removed workloads. It retains matching replicas, force-removes changed/excess/removed/stopped containers by ID, then creates missing replicas. Force removal makes no graceful-shutdown guarantee. Failed creates/starts/removals are reported, and subsequent passes observe and retry without trusting the previous action's outcome. Follow-up listings confirm convergence and allocated ports. Apply wakes the loop; periodic passes recover from backend outages and external stops. Image/port updates use replacement, with downtime and changing dynamic ports allowed.

## Live Docker acceptance

Prerequisites: the Rezn workspace built with the command above, a prebuilt sibling `../orqos/target/debug/orqos` (or `--orqos-bin /absolute/path`), Python 3, OpenSSL 3 and a local Docker context. Explicitly pre-pull the test images:

```bash
docker --context desktop-linux pull nginx:alpine
docker --context desktop-linux pull httpd:alpine
```

One reproducible acceptance command, from this repository:

```bash
python3 scripts/acceptance.py --docker-context desktop-linux
```

The script starts its own Docker-only Orqos and real Rezn processes on loopback ephemeral ports. It uses isolated temporary state/secrets databases and signing/age keys; waits are bounded. It performs signed HTTP create, unchanged reapply, scale `1 → 2 → 1 → 0`, image/port changes at equal replica count, removal of one pod, empty program, Rezn restarts, external stop recovery, and an outage/restart of its own Orqos process. It verifies nonzero published HTTP service, observed ports against Docker inspect, stable unchanged identities, no stopped leftovers and a foreign container with legacy name/labels staying untouched. It also checks signed unsupported intent changes neither state nor containers.

Cleanup runs on failures and success, verifies labels and removes only its owned fixtures and marked foreign fixture, then stops its own child processes. It never stops Docker Desktop or prunes resources. Image pulls are outside the acceptance script and production controller. Failure logs are printed before temporary artifacts are discarded. Ordinary CI needs none of these live prerequisites.

The full acceptance run passed locally on 2026-10-08 with the `desktop-linux` context (Docker 29.8.2), including process/backend restarts and HTTP on an observed allocated port.
