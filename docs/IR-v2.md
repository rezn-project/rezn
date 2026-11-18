# Rezn IR v2

Status: specified in M0 ([#32](https://github.com/rezn-project/rezn/issues/32)). `/apply` verifies, validates and
stores IR v2 programs, `reznctl validate` checks them offline ([#39](https://github.com/rezn-project/rezn/issues/39)),
and Rezn executes them through Orqos's workload backend ([#30](https://github.com/rezn-project/rezn/issues/30)) as far
as that backend reaches: [what runs today](DEVELOPMENT.md#what-runs) is replica sets and ordered replica sets with
their services, volumes, identity and hardening, and any other program is reported as unexecuted with the issue each missing feature needs. IR v1 is retired. The Kubernetes
import ([#83](https://github.com/rezn-project/rezn/issues/83)) follows in M1.

IR v2 is the signed program format that every front end produces: the Kubernetes manifest import (from the
[supported subset](KUBERNETES-SUBSET.md)) and the Rezn DSL. It is Rezn's own vocabulary with Kubernetes semantics
where they exist. Structure is defined by [`spec/ir-v2.schema.json`](../spec/ir-v2.schema.json); this document adds
the rules a schema cannot express. [`examples/ir-v2/`](../examples/ir-v2/) contains one program per kind and the
whole reference workload.

## Design rules

- **Reject, never drop.** Unknown keys, kinds and values are errors everywhere. Errors name a JSON pointer.
- **Canonical values.** CPU is integer millicores, sizes are integer bytes, durations are integer seconds, file
  modes are integers (`288` is `0440`). No quantity strings, so equal programs canonicalize (JCS) to equal bytes.
- **Names, not selectors.** Services, policies and ingresses name workloads and services directly. The
  Kubernetes import resolves label selectors to names and fails when a selector is ambiguous.
- **No secret values.** Programs declare secrets and reference their keys; values live in Rezn's secret store.

## Envelope

The `/apply` body keeps its v1 shape: `{"name": "<deployment>", "instruction_wrapper": {"program": …,
"signature": {"algorithm": "ed25519", "pub": "…", "sig": "…"}}}`. The signature still covers the JCS
canonicalization of `program`.

A v1 `program` is an array. A v2 `program` is an object:

```json
{
  "ir": "rezn.dev/v2",
  "deployment": "shop",
  "namespace": "shop",
  "resources": []
}
```

| Field | Rule |
| --- | --- |
| `ir` | Exactly `rezn.dev/v2`. |
| `deployment` | Name (below). Must equal the `/apply` `name`. Now covered by the signature. |
| `namespace` | Name. Scopes service DNS names (`<service>.<namespace>.svc`), policies and generated secrets. Two deployments may share a namespace only if their resource names do not collide. |
| `resources` | List of resources. An empty list removes every workload of the deployment. |
| `revision` | Reserved. Becomes required, and must increase per deployment, with replay protection ([#78](https://github.com/rezn-project/rezn/issues/78), M3). |

## Resources

Every resource is `{"kind", "name", "fields"}` with an optional `"phase"`.

**Names** are DNS-1123 labels: lowercase letters, digits and `-`, starting and ending with a letter or digit, at
most 63 characters. `ordered_replica_set` names are at most 52 characters, leaving room for `-<ordinal>` and
unit-name suffixes. Workload names (`replica_set`, `ordered_replica_set`, `job`, `scheduled_job`) are unique
across those kinds; other names are unique within their kind. A service may share its workload's name.

| Kind | Kubernetes equivalent | Purpose |
| --- | --- | --- |
| `replica_set` | Deployment (Recreate) | Interchangeable replicas, replaced in one go on change |
| `ordered_replica_set` | StatefulSet | Replicas with stable ordinals, hostnames and volumes |
| `job` | Job | Run to completion with a retry limit |
| `scheduled_job` | CronJob | A job on a schedule |
| `service` | Service | A stable DNS name and ports for one workload |
| `volume_claim` | PersistentVolumeClaim | A persistent volume shared by a workload's replicas |
| `config` | ConfigMap | Non-secret string data, mounted or referenced |
| `secret` | Secret | Declared secret keys and where their values come from |
| `ingress` | Ingress | HTTPS host and path routing at Rezn's edge |
| `network_policy` | NetworkPolicy | Default-deny traffic rules for workloads |

### `replica_set`

| Field | Rule |
| --- | --- |
| `replicas` | Integer ≥ 0. |
| `pod` | [Pod template](#pod-template). `restart` must be `always`. |

On any configuration change all replicas stop before new ones start (Kubernetes `Recreate`). Rolling replacement
is out of scope for v2.0.

### `ordered_replica_set`

| Field | Rule |
| --- | --- |
| `replicas` | Integer ≥ 0. Replica *N* is named and has the hostname `<name>-<N>`. |
| `service` | A headless `service` whose `target` is this set. Replica *N* resolves as `<name>-<N>.<service>`. |
| `pod_management` | `parallel` (start and stop replicas together) or `ordered` (one at a time, by ordinal). Default `ordered`. |
| `volume_claims` | List of `{"name", "bytes"}`. Each replica gets its own volume `<claim>-<name>-<N>`; pods mount it by `claim`. |
| `retain` | `{"on_scale_down": bool, "on_removal": bool}`, default both `true`. Retained volumes return to the same ordinal. |
| `pod` | Pod template. `restart` must be `always`. |

Configuration changes replace replicas from the highest ordinal down, each waiting for the previous one to be ready
([#45](https://github.com/rezn-project/rezn/issues/45)).

### `job`

| Field | Rule |
| --- | --- |
| `backoff_limit` | Integer ≥ 0, default 6. Failed runs are retried with exponential backoff up to this limit, then the job is failed. |
| `pod` | Pod template. `restart` must be `never` or `on_failure`. |

A job runs once per revision that changes it, or once per run of its phase.

### `scheduled_job`

| Field | Rule |
| --- | --- |
| `schedule` | Five-field cron expression, host local time. |
| `concurrency` | `forbid` (skip while a run is active), `replace` or `allow`. Default `allow`. |
| `keep_succeeded`, `keep_failed` | Integers ≥ 0: finished runs kept for inspection. Defaults 3 and 1. |
| `job` | `{"backoff_limit", "pod"}` as for `job`. |

### `service`

| Field | Rule |
| --- | --- |
| `target` | A `replica_set` or `ordered_replica_set` in the program. |
| `headless` | `true`: no shared address; each ready replica resolves individually. Default `false`. |
| `ports` | List of `{"name", "port", "target_port"}`. `target_port` is a number or the name of a container port in the target's pod. |

`<service>`, `<service>.<namespace>` and `<service>.<namespace>.svc` resolve inside the namespace to the target's
**ready** replicas only ([#59](https://github.com/rezn-project/rezn/issues/59)).

### `volume_claim`

`{"bytes": integer > 0}`. A persistent named volume, kept when the deployment is removed unless purged. Size
enforcement depends on the host ([#68](https://github.com/rezn-project/rezn/issues/68)).

### `config`

`{"data": {"<key>": "<string>"}}`. Keys match `[-._a-zA-Z0-9]{1,253}`. Mounted read-only or referenced by
environment variables. A change replaces the workloads that use it.

### `secret`

| Field | Rule |
| --- | --- |
| `type` | `opaque` or `tls`. A `tls` secret has exactly the keys `tls.crt` and `tls.key`. |
| `keys` | The key names workloads may reference. |
| `source` | Exactly one of the forms below. |

| Source | Meaning |
| --- | --- |
| `{"store": {}}` | Values are managed by the operator in Rezn's secret store. A revision whose referenced keys are missing fails before any workload changes. |
| `{"seed_once": {}}` | Values are written to the store once, out of band (for example by the Kubernetes import from rendered Secrets), and kept across revisions. |
| `{"random": {"<key>": {"length", "alphabet"}}}` | Generated once and kept: `length` characters from `alphanumeric` or `hex`. Covers every declared key. |
| `{"ca": {"common_name", "days"}}` | Generated once: a self-signed certificate authority (`type: tls`). |
| `{"certificate": {"ca", "dns_names", "days"}}` | Generated once: a certificate signed by the `ca`-sourced secret named `ca`, for the listed DNS names (`type: tls`); renewed before expiry. |

Generated and seeded values survive every later revision. They are removed only by an explicit uninstall purge
([#48](https://github.com/rezn-project/rezn/issues/48)).

### `ingress`

| Field | Rule |
| --- | --- |
| `hosts` | List of `{"host", "paths": [{"path", "service", "port"}]}`. `path` is an absolute prefix; `port` is a port name of that service. |
| `tls` | List of `{"hosts", "secret"}`; `secret` is a `tls` secret. |

Rezn's edge terminates TLS and forwards plain HTTP ([#60](https://github.com/rezn-project/rezn/issues/60)).

### `network_policy`

| Field | Rule |
| --- | --- |
| `targets` | Workloads the policy applies to. |
| `ingress` | Optional list of `{"from": [peer], "ports": [port]}`. When present, inbound traffic to the targets is denied unless a rule allows it; `[]` denies all. |
| `egress` | Optional, same shape with `to`. When present, outbound traffic is denied unless allowed. DNS is not implicit. |

A peer is `{"workload": "<name>"}` or `{"cidr": "<IPv4 CIDR>"}`. A port is `{"port", "protocol": "tcp" | "udp"}`.
An omitted `from`/`to` means any peer; omitted `ports` means any port. Enforced by Orqos with owned nftables rules
([orqos#30](https://github.com/orqos-project/orqos/issues/30)).

## Pod template

| Field | Rule |
| --- | --- |
| `containers` | One or more containers. |
| `init` | Containers run once, in order, before sidecars and containers start. A failure fails the pod start. |
| `sidecars` | Containers started before `containers`, kept running and restarted, stopped after them. If a sidecar has a `startup` probe, the main containers start only after it passes. |
| `restart` | `always`, `on_failure` or `never`. Default `always`. |
| `grace_seconds` | Seconds between SIGTERM and SIGKILL. Default 30. |
| `user`, `group` | Default UID/GID for every container. |
| `run_as_non_root` | `true` rejects a pod whose effective user is 0. |
| `fs_group` | GID that owns new volumes and projected files (group-writable volumes). |
| `fs_group_policy` | `on_root_mismatch`: ownership is changed only when a volume root does not match. The only value. |
| `seccomp` | `runtime_default`. The only value; also the default. |
| `image_pull_secrets` | Secrets with registry credentials. |
| `volumes` | List of volumes, below. |

Names of `init`, `sidecars` and `containers` are unique within the pod; port names are unique within the pod.

### Volumes

Each volume is `{"name", <one source>}`:

| Source | Rule |
| --- | --- |
| `"scratch": {"medium": "disk" \| "memory", "bytes"}` | Empty at pod start, shared by every container of the pod, removed when the pod is removed. `bytes` is required and enforced for `memory`. The volume lives as long as the pod, including across init containers. |
| `"claim": "<name>"` | A `volume_claim`, or one of the enclosing `ordered_replica_set`'s `volume_claims`. |
| `"config": {"name", "items"?, "mode"?}` | Files from a `config`. `items` (`[{"key", "path"}]`) selects keys; default all keys under their names. Mode default 420 (`0644`). |
| `"secret": {"name", "items"?, "mode"?}` | Files from a `secret`. Owner `root`, group `fs_group` when set. Mode default 420. |

## Containers

| Field | Rule |
| --- | --- |
| `name` | Name. |
| `image` | Image reference in Rezn's grammar ([DEVELOPMENT.md](DEVELOPMENT.md#executable-intent-and-apply-contract)), with tag or `@sha256:` digest. |
| `pull` | `if_not_present`, `always` or `never`. Default `if_not_present`. |
| `command`, `args` | Lists of strings, replacing the image's entrypoint and command. |
| `env` | List of `{"name", "value"}`, `{"name", "secret": {"name", "key"}}` or `{"name", "pod_ip": true}`. Names are unique. |
| `ports` | List of `{"name", "port"}` (TCP). |
| `startup`, `readiness`, `liveness` | Probes, below. |
| `resources` | `cpu_request_millis`, `cpu_limit_millis`, `memory_request_bytes`, `memory_limit_bytes`. Limits become cgroup limits; requests are admission checks against host capacity ([#40](https://github.com/rezn-project/rezn/issues/40)). |
| `security` | `user`, `group`, `run_as_non_root` (override the pod), `read_only_root`, `allow_privilege_escalation` (`false` only), `drop_capabilities` (`["all"]` only). No capability can be added. |
| `mounts` | List of `{"volume", "path", "read_only"?, "sub_path"?}`. `sub_path` is relative and contains no `..`. |

### Probes

`{"exec": ["cmd", …]}` or `{"http": {"path", "port", "scheme": "http" | "https"}}`, plus `period_seconds`
(default 10), `timeout_seconds` (default 1) and `failure_threshold` (default 3). HTTP probes run from outside the
container, inside the pod's network namespace, so images need no `curl`; `https` does not verify certificates.

- `startup`: other probes start only after it passes; failing it restarts the container.
- `readiness`: decides service membership and rollout progress; failing it never restarts the container.
- `liveness`: failing it restarts the container.

## Phases

`"phase": {"when": "before" | "after", "on": ["install", "upgrade"], "weight": integer}` orders a resource around
the main apply of a revision (Helm hooks). `on` lists `install`, `upgrade` or both.

1. Unphased `config`, `secret`, `volume_claim`, `service`, `network_policy` and `ingress` are applied.
2. `before` phases run in ascending weight. Each must finish (jobs complete, workloads ready) before the next.
3. Unphased workloads are applied.
4. `after` phases run in ascending weight.

A phased job runs each time its phase runs, replacing the previous run. A phased resource that is not a job is
created when its phase first runs and is **not changed by later revisions** whose phase does not include it, and is
kept when the deployment is removed unless purged. A failing `before` phase leaves the previous revision's
workloads running ([#44](https://github.com/rezn-project/rezn/issues/44)).

## Configuration identity

A workload's configuration identity is the SHA-256 of the JCS canonicalization of its pod template (and, for
`ordered_replica_set`, `volume_claims` and `service`), together with the digests of every `config` it references
and the stored versions of every secret key it references. `replicas`, `retain` and `pod_management` are excluded.
A changed identity replaces the workload; an unchanged one preserves its containers
([#37](https://github.com/rezn-project/rezn/issues/37)).

## Validation beyond the schema

- `deployment` equals the `/apply` name; names are unique as stated above.
- Every reference resolves: service `target` and `target_port` names, `ordered_replica_set.service` (headless,
  targeting the set), volume `claim`, `config` and `secret` names, `items` keys (declared `keys` / `data` keys),
  env secret keys, `image_pull_secrets`, mount `volume`s, probe port names (a port of the same container),
  ingress services, ports and `tls` secrets, policy workloads, certificate `ca` secrets.
- `replica_set` and `ordered_replica_set` pods use `restart: always`; `job` pods use `never` or `on_failure`.
- `run_as_non_root` pods have a non-zero effective user for every container.
- A container's `cpu_request_millis` and `memory_request_bytes` are at most its limits when both are set.
- `tls` secrets declare exactly `tls.crt` and `tls.key`; a `random` source covers every declared key.
- `image_pull_secrets` name secrets that declare the key `.dockerconfigjson` (registry credentials in Docker's
  configuration format).

## Execution on systemd and Podman (informative)

Following [ADR 0001](adr/0001-execution-foundation.md): each pod is a Podman pod run by owned Quadlet units;
`init` containers are one-shot units, retried in place on failure, `sidecars` and `containers` are services ordered
after them, and a sidecar's `startup` probe gates the main containers: they wait at an `ExecStartPre` gate that
Orqos's prober opens once every sidecar startup probe has passed (probes are Orqos's, not Podman healthchecks, so
`Notify=healthy` is not used). `scratch` volumes are pod-scoped, not per-container tmpfs mounts, because Podman
tmpfs volumes vanish when no container mounts them. Every restart carries explicit backoff.

Each namespace is one Podman network with aardvark-dns, and services are network aliases on their target's pods
([networking](DEVELOPMENT.md#networking-and-services)): a non-headless service resolves to every ready replica
(DNS round-robin) until virtual addresses and port remapping
([#123](https://github.com/rezn-project/rezn/issues/123)) land, and namespaces cannot reach each other. Each pod
has a user namespace of 65536 IDs, and volumes are mounted idmapped, so files keep the IDs the container sees
whatever host range the pod gets ([storage](DEVELOPMENT.md#storage-and-identity)). `config` and `secret`
volumes are files Orqos writes per pod and bind-mounts read-only and idmapped; each config and secret is an
immutable Orqos object whose ID covers its content (the config's data or the stored key versions), which is how a
changed input replaces exactly the workloads that use it ([configs and
secrets](DEVELOPMENT.md#configs-and-secrets)). `ca` and `certificate` secrets are ECDSA P-256; certificates are
renewed once two thirds of their lifetime have passed, and a CA is not rotated in v2.0. Probes are run by Orqos itself rather than as
Podman healthchecks, which allow one check per container and run inside it; resource limits are cgroup limits
set through Podman and requests are admitted against the host's capacity minus a reserve
([admission](DEVELOPMENT.md#resources-and-admission)); HTTP probes connect from a thread that
enters the pod's network namespace. Until readiness-aware membership
([#59](https://github.com/rezn-project/rezn/issues/59)), services resolve to running replicas.

## Out of scope for v2.0

Rolling replacement of `replica_set`, autoscaling, multi-node placement, gRPC and TCP probes, capability
additions, host namespaces, and any Kubernetes field outside the [supported subset](KUBERNETES-SUBSET.md). Each
needs a new IR version: the `ir` string is matched exactly.
