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

Orqos's workload backend runs pods of containers (image, `command`, `args`, literal and secret environment values,
user and groups, hardening, restart policy and grace period) on owned networks with DNS aliases, with owned,
scratch and secret-file volumes. A deployment runs only when **every** resource maps exactly onto that:

- `replica_set` and `ordered_replica_set` resources, each replica as one Orqos pod: one Podman pod with its own
  network namespace, user namespace and hostname `<name>-<ordinal>`;
- `volume_claim` resources and ordered sets' `volume_claims`, pod volumes from `claim` and `scratch`, and container
  `mounts` (below);
- `user`, `group`, `fs_group`, `fs_group_policy: on_root_mismatch`, `seccomp: runtime_default`,
  `run_as_non_root` with an explicit non-zero user, and container `security` (`user`, `group`, `read_only_root`,
  `allow_privilege_escalation: false`, `drop_capabilities: ["all"]`);
- `service` resources whose every `port` equals its target port (a number, or the named container port it
  resolves to), as DNS names (below);
- `config` and `secret` resources (every source: `store`, `seed_once`, `random`, `ca` and `certificate`), as
  `config` and `secret` pod volumes and `secret` environment references (below);
- containers with `pull` absent or `if_not_present`, environment `value`s, and declared `ports` (informational, as in
  Kubernetes: nothing is published on the host).

Anything else leaves the whole deployment **unexecuted**: services that remap ports
([#123](https://github.com/rezn-project/rezn/issues/123)), ingress, network policies, jobs, phases, init containers and sidecars, probes,
resource requests and limits, other pull policies, registry credentials, pod-IP environment references, a group
without a user or `run_as_non_root` without an explicit user (both need the image's user), IDs above 65535, a claim
mounted by pods with different `fs_group`s, and referenced secret keys missing from the secret store.
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

### Storage and identity

Every pod has its own user namespace of 65536 IDs (`UserNS=auto:size=65536`), so container root and every container
ID map to an unprivileged host range. That range can differ after a restart, so volumes are mounted **idmapped**:
on disk, files keep the IDs the container sees (user 1000 writes files owned by 1000), and whichever range the pod
gets next can read them. Containers run as the container's `security.user`/`group`, else the pod's, else the
image's user; `fs_group` is added as a supplementary group.

- A `volume_claim` is one Orqos volume shared by every pod that mounts it.
- An ordered set's `volume_claims` give each replica its own volume (`<claim>-<set>-<N>` in IR terms), which
  follows the ordinal across restarts, replacement and scaling.
- A volume's root belongs to `root:<fs_group>` with mode 2775 (setgid, so new files get the group), or
  `root:root` 0755 without `fs_group`. The group is set when the volume is created and set again only if the root's
  group differs (`on_root_mismatch`), never recursively.
- `scratch` volumes belong to one pod: `memory` is a tmpfs of `bytes`, `disk` a local volume (`bytes` advisory,
  [#68](https://github.com/rezn-project/rezn/issues/68)). They are empty when the pod is created or replaced, kept
  across container restarts, and removed with the pod.
- Mounts honour `read_only` and `sub_path`.

**Retention.** Volumes are created before the pods that mount them and removed only once no pod mounts them. A
per-replica volume beyond the replica count stays unless the set says `retain.on_scale_down: false`; when its set or
deployment is gone it stays unless `retain.on_removal` was `false` (the last policy is kept with the volume in
Orqos). A `volume_claim` is always kept when its deployment is removed. Retained volumes come back to the same claim
and ordinal, and an unexecuted deployment's volumes are never removed. Deleting retained volumes is an operator
action for now, through Orqos (`DELETE /workload/volumes/<id>`); a Rezn purge belongs with uninstall
([#48](https://github.com/rezn-project/rezn/issues/48)).

### Configs and secrets

Secret values never enter programs. A `secret` resource names its keys and a source:

- `store` values are set by an operator, per namespace: `PUT /namespaces/<namespace>/secrets/<name>` with
  `{"data": {"<key>": "<value>"}}` replaces every stored value of that secret. `GET /namespaces/<namespace>/secrets`
  lists stored and declared secrets with each key's `version`, the declaring deployments, the `source` and the
  declared keys still `missing`; values never come back. `DELETE` removes stored values once no program declares
  the secret.
- `seed_once` values are written the same way, but only for keys that have none yet (409 otherwise).
- `random` values are generated by `/apply` from the OS RNG and kept across every later revision; they cannot be
  set (409).
- `ca` and `certificate` secrets are generated too (below); they cannot be set either.

`/apply` refuses (400) a program that references a `store` or `seed_once` key the store lacks, naming each key and
the PUT that fixes it, and one whose stored value would become an environment variable with control characters.
Programs' secrets live in their own trees of the age-encrypted secret store (the legacy `/secrets` routes see only
their own keys), keyed by namespace, secret and key, with a **version** per key: an HMAC-SHA256 of the value
under a random key kept encrypted in the store. Rewriting the same value keeps its version; the version reveals
nothing about the value. Stored values stay when a deployment is removed until deleted (purge with uninstall is
[#48](https://github.com/rezn-project/rezn/issues/48)).

Each `config` and `secret` a pod uses becomes an **Orqos secret** (orqos#24), immutable and holding the config's
data or the secret's declared keys from the store. Its ID covers its content (the config's data, or the secret's
key versions), so a changed config or stored value is a new object: exactly the pods that use it get a new spec
and are replaced (one replica at a time, as for any change), and nothing else is. `/status` names the cause on each
pod that is not current: `config "<name>"`, `secret "<name>"` or `pod template`.

- `config` and `secret` volumes are written by Orqos as files owned by root and `fs_group` (or root) with the
  volume's `mode` (default 0644; directories get `r-x` where files get `r`), selected by `items` or all keys,
  and mounted **read-only**, idmapped, replacing the mount path's directory (or one file with `sub_path`).
- `secret` environment references go into the container's root-only environment file, never into a spec. Values
  must be UTF-8 without control characters.
- Orqos secrets are created before the pods that use them and removed in the same pass once no pod uses them.
  Orqos keeps their values root-only on disk, unencrypted (like Kubernetes' default etcd).

**Certificates.** A `ca` secret is a self-signed authority (`CN=<common_name>`, `CA:TRUE, pathlen:0`, key usage
certificate and CRL signing), generated by the first `/apply` that declares it and then **never regenerated**:
changing its `common_name` or `days` later is reported as `spec_changed` in the listing, because a new CA would
invalidate every certificate it signed and every trust bundle (rotation is a follow-up). A `certificate` secret is
signed by its `ca` for `dns_names` (SANs, `CN=` the first name), for server and client authentication, valid for
`days` but never beyond its CA. It is issued again when it is missing, when its `ca`, `dns_names` or `days` change
(at `/apply`), when its CA's certificate changes, or once **two thirds of its lifetime** have passed (checked at
the start of every reconcile pass). A new certificate has new key versions, so its users roll like for any changed
input. Keys are ECDSA P-256 in PKCS#8 PEM (`tls.key`); `tls.crt` holds the leaf only. Workloads trust a CA by
mounting its certificate alone, `{"secret": {"name": "ca", "items": [{"key": "tls.crt", "path": "ca.crt"}]}}`:
an Orqos secret holds only the keys its deployment's pods reference, so the CA's private key never leaves Rezn.
The namespace secret listing shows each generated secret's `certificate` (`kind`, `not_before`, `not_after`,
`renew_after`, `spec_changed`), never its values.

## Reconciliation

One serialized loop runs after every apply and every `RECONCILE_INTERVAL`. Each pass first renews due
certificates (above), then:

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
4. creates volumes and Orqos secrets before the pods that use them, then observes again, removes volumes and
   Orqos secrets that the changes released (when their policy allows), and succeeds only when the executed
   deployments match.

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
| `volumes[]` | Owned volumes: `id`, `deployment`, `claim`, `workload` and `ordinal` (per-replica volumes), `desired` (wanted by the accepted programs; otherwise retained or awaiting deletion), `retain`, `exists` and `mounted_by` |
| `pods[]` | `id`, `ordinal`, `hostname`, Orqos's `state` (`running`, `starting`, `restarting`, `degraded`, `failed`, `stopped`), `current` (runs the desired spec), `changes` (why it is not current: `config "<name>"`, `secret "<name>"`, `pod template`), `aliases` (its DNS names on the namespace network), `containers[]` (`name`, `active_state`, `sub_state`, `result`, `restarts`) and Orqos's last `error` |

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

Each volume is an Orqos volume named `rz-<first 24 hex digits of sha256([owner, "volume", deployment, set or null,
claim, ordinal or null])>`, with these labels in Orqos's volume record (where they can change, unlike Podman's):

```text
dev.rezn.owner, dev.rezn.deployment, dev.rezn.namespace
dev.rezn.claim=<claim name>
dev.rezn.workload=<ordered set>, dev.rezn.ordinal=<replica number>   (per-replica volumes only)
dev.rezn.retain=true | false
```

Volume ownership is proven the same way: the labels must be well formed and produce the volume's name.

Each Orqos secret is named `rz-<first 24 hex digits of sha256([owner, kind, deployment, name, content])>` and
labelled `dev.rezn.owner`, `dev.rezn.deployment`, `dev.rezn.namespace`, `dev.rezn.kind` (`config` or `secret`),
`dev.rezn.resource` and `dev.rezn.content` (the content digest); its ownership is proven the same way.

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
- pods run as their user with `fs_group`-owned volumes, a read-only root and no capabilities; files keep
  container IDs on disk, and a shared claim is shared;
- scaling an ordered set 2 → 1 → 2 brings replica 1 back with its own volume;
- replacing every replica keeps each one's volume and starts its scratch volume empty;
- removing a deployment keeps its volumes, and applying it again reuses them;
- an apply referencing a missing stored secret is refused; config and secret files reach pods as read-only
  `root:<fs_group>` files with their modes, and stored and generated secrets become environment variables;
- a random secret is generated once and kept, and a changed config replaces only the pods that use it;
- storing the same secret value changes nothing, and a new value replaces only the pods that use it;
- generated secrets cannot be overwritten, declared ones cannot be deleted, and no value appears in Rezn's or
  Orqos's responses;
- removing the deployment removes its Orqos secrets and keeps stored values until deleted;
- a generated CA signs a certificate for a service's names, and a client trusting only the CA's certificate
  completes a verified TLS handshake by name (`openssl s_client`), while the CA key never reaches Orqos;
- an unchanged re-apply keeps the certificate; adding a name reissues it from the same CA and replaces only the
  server;
- the listing shows certificate validity without values, generated certificates cannot be set, and they outlive
  their deployment until deleted;
- empty programs remove every owned pod and then the namespace networks, while a foreign Orqos pod stays.

It removes everything it created, including after a failure. The *Acceptance* workflow runs it on Ubuntu 26.04
against the Orqos commit pinned in `.github/workflows/acceptance.yml`.
