# ADR 0001: Execution foundation

- Status: proposed
- Date: 2026-11-17
- Issue: [#29](https://github.com/rezn-project/rezn/issues/29); epic [#26](https://github.com/rezn-project/rezn/issues/26)
- Evidence: [systemd + Podman spike](../spikes/systemd-podman/README.md) ([#27](https://github.com/rezn-project/rezn/issues/27)),
  [`podman kube play` spike](../spikes/kube-play/README.md) ([#28](https://github.com/rezn-project/rezn/issues/28))

## Context

Rezn aims to replace Kubernetes for one Linux host first, starting with the reference workload described in the
[roadmap](https://github.com/rezn-project/rezn/issues/25). That workload needs pods of several containers (init
containers, a sidecar that must be ready before the main container), stable per-replica hostnames and volumes,
run-to-completion and scheduled jobs, ordering on readiness across workloads, secret files with exact owners and
modes, a hardened security context, service DNS names, network policy, graceful stop and crash-loop backoff.
Running workloads must survive Rezn and Orqos being down.

Rezn currently drives Docker containers through Orqos's Docker API.

## Options

| Option | Assessment |
| --- | --- |
| **A. Docker through Orqos** (status quo) | No pods, no supervision ordering, no jobs or timers, no secret files with owner and mode. Rezn would have to build a supervisor on top of Docker and keep it running for workloads to be restarted. |
| **B. systemd + Podman (Quadlet units) through Orqos** | Pods with hostnames and shared network namespaces, one-shot init units, health-gated start (`Notify=healthy`), unit ordering, restart with exponential backoff, cgroup limits, timers, Podman secrets with owner and mode, networks with DNS aliases. systemd supervises independently of Rezn. Verified in #27. |
| **C. `podman kube play`** | Silently drops StatefulSets and CronJobs, ignores ordering, Service names and NetworkPolicies, and shares `emptyDir` volumes across pods (#28). Not viable as an executor. |
| **D. containerd + CNI** | A kubelet rewrite: images, sandboxes, networking, supervision and logs all built in Rezn. Highest cost, no supervision independent of Rezn. |
| **E. Embed a Kubernetes distribution** (k3s) | Works, but makes Rezn a Kubernetes wrapper, which defeats the goal. |

## Decision

Adopt **B**: Orqos renders and manages owned Quadlet units for Rezn's workloads, systemd supervises them and Podman
runs them. Rezn renders Quadlet units from IR v2 itself; it does not use `podman kube play`.

Responsibilities:

| Layer | Owns |
| --- | --- |
| Rezn | Signed desired state (IR v2), validation, revision phases and gates, generate-once secrets and certificates, readiness-aware service membership, rollout policy, status |
| Orqos | Rendering and atomically installing owned units, `daemon-reload` and targeted restarts, owned Podman resources (pods, volumes, networks, secrets, files), image pulls, probes, observation of real resources, nftables network policy, recovery after restart and reboot |
| systemd | Supervision, ordering, restart backoff, stop signals and timeouts, cgroup limits, timers |
| Podman | Images, pods, network namespaces and DNS, volumes, secrets, health checks |

Minimum versions: Linux with systemd ≥ 254 (`RestartSteps=`, `RestartMaxDelaySec=`) and cgroup v2, Podman ≥ 5.0
(Quadlet `.pod` units). Verified on Ubuntu 26.04 (systemd 259, Podman 5.7.0). Ubuntu 24.04 ships Podman 4.9 and
is not supported for this backend. Orqos's existing Docker and Kubernetes APIs stay unchanged.

## Consequences

Design requirements recorded by #27:

1. Pod-scoped memory volumes need the pod's lifetime; Podman tmpfs volumes vanish when no container mounts them
   ([#34](https://github.com/rezn-project/rezn/issues/34), [orqos#21](https://github.com/orqos-project/orqos/issues/21)).
2. Unit state is a hint; Orqos observes real Podman resources and resets failed units deliberately
   ([orqos#27](https://github.com/orqos-project/orqos/issues/27)).
3. Every restart policy carries explicit backoff ([#38](https://github.com/rezn-project/rezn/issues/38)).
4. `Notify=healthy` gates start only; ongoing readiness is watched by Orqos/Rezn
   ([#59](https://github.com/rezn-project/rezn/issues/59), [orqos#26](https://github.com/orqos-project/orqos/issues/26)).
5. Boot behaviour is explicit (`[Install]` or Orqos-driven start) ([orqos#28](https://github.com/orqos-project/orqos/issues/28)).
6. Applying a revision is write units atomically → `daemon-reload` → restart exactly the affected units
   ([orqos#17](https://github.com/orqos-project/orqos/issues/17)).
7. Named volume sizes are not enforced by Podman ([#68](https://github.com/rezn-project/rezn/issues/68)).

From #28: the manifest import ([#83](https://github.com/rezn-project/rezn/issues/83)) must reject unsupported kinds and
fields, and must not map `emptyDir` the way `podman kube play` does.

## Open questions

- **Rootful or rootless.** Both spikes ran rootless and every check passed; rootful is closer to Orqos host mode and
  simplifies nftables policy. Decide in [orqos#16](https://github.com/orqos-project/orqos/issues/16) with
  [orqos#33](https://github.com/orqos-project/orqos/issues/33) as the rootless follow-up.
- **Several ordinals.** Quadlet template units (`name@.container`, `name@.pod`) are the candidate mechanism for
  ordered replica sets ([#36](https://github.com/rezn-project/rezn/issues/36)); not yet tried.
- **Resource limits and network policy** were not exercised by the spikes.
