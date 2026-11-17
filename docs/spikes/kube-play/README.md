# Spike: how much of a real Helm chart `podman kube play` runs

Issue: [#28](https://github.com/rezn-project/rezn/issues/28). Run on 2026-11-17 with Podman 5.7.0 (rootless) and
Helm 4.3.2. Results feed the foundation ADR ([#29](https://github.com/rezn-project/rezn/issues/29)).

## Method

The reference workload's chart was rendered with `helm template` (evaluation profile, every optional job enabled):
41 objects. Every image was replaced with `alpine:3.22`, because the question is which objects and fields Podman
creates, not whether the application runs. The result was passed to `podman kube play --start=false`, then each pod,
container, volume and network was inspected and torn down.

| Kind | Objects |
| --- | --- |
| StatefulSet | 5 (database, object store, three worker pools) |
| Deployment | 3 |
| Job | 3 (two Helm hooks, one manual) |
| CronJob | 1 |
| Service | 8 (ClusterIP and headless) |
| NetworkPolicy | 4 |
| Secret / ConfigMap / PersistentVolumeClaim | 10 / 4 / 3 |

## Results

`podman kube play` exited 0 and printed **no warning**, yet created only 6 pods from the 12 workload objects.

| Feature | Result |
| --- | --- |
| Deployment | Created as a pod named `<name>-pod`; `strategy` has no meaning |
| Job | Created as a pod; created together with everything else |
| **StatefulSet** | **Silently dropped** (not a supported kind): no database, object store or worker pools |
| **CronJob** | **Silently dropped** |
| Helm hooks and weights | No meaning: the migration job, its database and the API are created at once |
| Service | Not turned into DNS names; pods resolve only container and pod names, so `<service>` and `<service>.<namespace>.svc` do not resolve |
| NetworkPolicy | Ignored |
| Secret, ConfigMap (including a `subPath` file mount), PersistentVolumeClaim | Created; PVC size ignored |
| Init containers | Created |
| `runAsUser/Group`, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]` | Mapped |
| CPU and memory limits | Mapped |
| Liveness probe (`httpGet`) | Becomes a health check running `curl` **inside** the container, which needs curl in the image |
| Readiness and startup probes | Startup mapped to a startup health check; readiness ignored (T2 below) |
| `terminationGracePeriodSeconds` (default 30) | Stop timeout 10 s |
| **`emptyDir`** | **Becomes a named volume called after the volume, shared by every pod using that name**; `sizeLimit` ignored |

### `emptyDir` sharing (reproduction)

The manual says an `emptyDir` becomes an anonymous volume deleted with the pod. On Podman 5.7.0 two unrelated pods
that each declare an `emptyDir` named `tmp` mount the same named volume `tmp`:

```yaml
apiVersion: v1
kind: Pod
metadata: { name: rezn-mini-a }
spec:
  containers: [{ name: c, image: docker.io/library/alpine:3.22, command: [sleep, "1"], volumeMounts: [{ name: tmp, mountPath: /tmp }] }]
  volumes: [{ name: tmp, emptyDir: { sizeLimit: 64Mi } }]
---
apiVersion: v1
kind: Pod
metadata: { name: rezn-mini-b }
spec:
  containers: [{ name: c, image: docker.io/library/alpine:3.22, command: [sleep, "1"], volumeMounts: [{ name: tmp, mountPath: /tmp }] }]
  volumes: [{ name: tmp, emptyDir: {} }]
```

```text
$ podman kube play --start=false mini.yaml
$ podman inspect rezn-mini-a-c rezn-mini-b-c --format '{{range .Mounts}}{{.Type}} {{.Name}} -> {{.Destination}}{{end}}'
volume tmp -> /tmp
volume tmp -> /tmp
```

Scratch data therefore leaks between workloads and outlives its pod.

## Behaviour matrix

Twelve minimal manifests ([`matrix/`](matrix/)), each played with Podman 5.7.0 rootless, inspected at runtime and
removed. Source references are to `containers/podman` at tag v5.7.0 (the project now lives at
`podman-container-tools/podman`): `play.go` = `pkg/domain/infra/abi/play.go`, `kube.go` =
`pkg/specgen/generate/kube/kube.go`, `volume.go` = `pkg/specgen/generate/kube/volume.go`.

| # | Feature | Observed | Why |
| --- | --- | --- | --- |
| T1 | Disk `emptyDir` | Named volume called after the volume name; **shared across pods** (pod B read pod A's file); `sizeLimit` ignored (whole disk available); removed with the pod | Named after the Kubernetes volume name (`kube.go:583-591`, `volume.go:265-299`); libpod reuses an existing volume of that name (`libpod/runtime_ctr.go:503-517`); `SizeLimit` is never read |
| T1 | `emptyDir: {medium: Memory}` | A separate tmpfs **per container**, not shared even within the pod; `sizeLimit` ignored (half of RAM) | Per-container tmpfs mount (`kube.go:592-598`) |
| T2 | Exec liveness / startup probes | Mapped to a health check and a startup health check | `probeToHealthConfig` (`kube.go:709-819`) |
| T2 | Readiness probe | **Ignored**; a container that is never ready reports healthy | Not referenced except to reject it on init containers |
| T2 | `httpGet` / `tcpSocket` probes | Become `curl -f …` / `nc -z …` run **inside** the container; without curl the liveness check fails and, with restart policy Always, the container is restarted repeatedly | `kube.go:741`, `kube.go:749`, failure action `restart` (`kube.go:769-787`); gRPC probes are dropped |
| T3 | Init containers | Run in order before the main container, then removed | `libpod/pod_api.go:20-58`; run mode from one object-wide annotation `io.podman.annotations.init.container.type` |
| T4 | Failing init container | `kube play` exits 125; main container left created, never started; no retry | |
| T5 | Native sidecar (init container with `restartPolicy: Always` + `startupProbe`) | **Rejected**: "cannot create an init container that has either of lifecycle, livenessProbe, readinessProbe, or startupProbe set" | `play.go:951-954` |
| T5 | Same sidecar without a probe | `restartPolicy` silently dropped; the sidecar **blocks** like an ordinary init container; main started only after it exited; the sidecar does not run beside main | The vendored `Container` type has no `restartPolicy` field |
| T6 | `terminationGracePeriodSeconds: 25` | Stop timeout 25 s, honoured by `kube down` | `play.go:1095-1097`; when unset, Podman's default 10 s applies, not Kubernetes' 30 s |
| T7 | Deployment `replicas: 3` | One pod, with a warning "Limiting replica count to 1" | `play.go:578-587` |
| T8 | Job with `backoffLimit: 2`, `restartPolicy: OnFailure` | Restart policy `on-failure` with **no limit**: 42 restarts in 12 s, never reported failed | `backoffLimit`, `completions`, `parallelism`, `activeDeadlineSeconds` never read |
| T9 | `runAsUser/Group`, `supplementalGroups`, `capabilities.drop: [ALL]`, `allowPrivilegeEscalation: false`, read-only root | Mapped (`CapEff 0`, `NoNewPrivs 1`, seccomp filter on) | `kube.go:954-1028` |
| T9 | `fsGroup`, `fsGroupChangePolicy`, `runAsNonRoot` | **Ignored**: volume owned by the user, the fsGroup GID never added | Not referenced; `seccompProfile` also ignored (only deprecated annotations), the default profile applies anyway |
| T10 | `secretKeyRef` env | Works | `kube.go:1069-1153`; `envFrom` overrides `env`, the opposite of Kubernetes |
| T10 | Secret volume `defaultMode: 0440` | Honoured, but files are **root:root**, so a non-root workload cannot read them | Volume written at play time (`play.go:785-832`) |
| T10 | Two volumes from the same Secret, one with `items` and `mode: 0400` | **Both mounts show the same merged directory**; item `mode` ignored | Secret and ConfigMap volumes are named after the Secret/ConfigMap and reused |
| T11 | DNS | Pod name, `hostname` and each container's bare name resolve; `hostname.subdomain`, Service names and `<svc>.<ns>.svc` do not | `play.go:888-899`, `libpod/networking_common.go:82-90`; bare container names collide across pods |
| T12 | Pod `restartPolicy: Always`, crashing container | Restarted immediately every time: 13 restarts in 30 s, **no backoff** | Podman restart policy `always` |
| T13 | Job-only manifest with `--service-container=true` (what Quadlet `.kube` units pass) | **Panic**: nil pointer dereference | A service container is created only for Pod and Deployment (`play.go:330`) but used unconditionally (`play.go:505`) |

Unsupported kinds and unknown fields are skipped at **info** log level (`play.go:479-481`), below Podman's default
`warn`, so nothing is printed.

### Quadlet `.kube` units

A `.kube` unit runs `podman kube play --replace --service-container=true` and `podman kube down` on stop. It adds
network-online ordering and a service container as the unit's main process, but no `Restart=` and no in-place
update: every start force-removes and recreates the pods, which also resets every `emptyDir`. Container restarts
are left to Podman's restart policy (default Always, no backoff).

### What changed upstream

Podman 6.2.0 (released 2026-11-10; Ubuntu 26.04 ships 5.7.0) adds `--validate=ignore|warn|strict` (default still
`ignore`), `--multiple-pods` for Deployment replicas, a pod-shared tmpfs for `medium: Memory` honouring its
`sizeLimit`, `securityContext.seccompProfile`, and `env` precedence over `envFrom`. Still unsupported on the main
branch: StatefulSet, CronJob, Service and NetworkPolicy, readiness probes, native sidecars, `fsGroup`, Job retry
limits, disk `emptyDir` size limits, and the volume naming behind T1 and T10. Open upstream items include #27712
(warn on unsupported kinds), #13538 (NetworkPolicy), #12417 (readiness), #20372/#25157 (sidecars), #17583
(fsGroup), #29670 (init-container data in tmpfs `emptyDir` lost) and #18318 (curl/nc probe dependency). No upstream
issue was found for cross-pod `emptyDir` sharing (T1) or the Job-only service-container panic (T13).

## Conclusion

`podman kube play` is not a viable executor for the reference workload: the stateful half of the chart disappears
without a warning, ordering and service names are lost, and scratch volumes are shared across pods. Rezn should
render Quadlet units itself, as in the [systemd + Podman spike](../systemd-podman/README.md), and treat kube play
only as a reference for field mappings. Rezn's import must reject what it does not support
([#83](https://github.com/rezn-project/rezn/issues/83)) instead of dropping it.
