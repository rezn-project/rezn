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
| Readiness and startup probes | Not observed in the created containers |
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

## Conclusion

`podman kube play` is not a viable executor for the reference workload: the stateful half of the chart disappears
without a warning, ordering and service names are lost, and scratch volumes are shared across pods. Rezn should
render Quadlet units itself, as in the [systemd + Podman spike](../systemd-podman/README.md), and treat kube play
only as a reference for field mappings. Rezn's import must reject what it does not support
([#83](https://github.com/rezn-project/rezn/issues/83)) instead of dropping it.
