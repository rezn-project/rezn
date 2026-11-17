# Spike: systemd + Podman as Rezn's execution foundation

Issue: [#27](https://github.com/rezn-project/rezn/issues/27). Run on 2026-11-17 on Ubuntu 26.04.1, systemd 259,
Podman 5.7.0 (netavark, runc, cgroup v2), **rootless** as systemd user units. Results feed the foundation ADR
([#29](https://github.com/rezn-project/rezn/issues/29)).

## What it models

Hand-written Quadlet units in [`units/`](units/) for the shape of a stateful worker that needs Kubernetes today:

| Kubernetes | Here |
| --- | --- |
| StatefulSet replica `pg-0` with a volume claim and a Service | Pod `rezn-spike-pg-0` with `HostName=pg-0`, aliases `pg` and `pg.spike.svc`, volume `rezn-spike-pg-0` |
| StatefulSet replica `worker-0` (ordinal = identity) | Pod `rezn-spike-worker-0` with `HostName=worker-0` |
| Init container preparing owner-only directories | `Type=oneshot` container unit, ordered before the sidecar |
| Native sidecar with a startup probe | Container unit with `Notify=healthy`: systemd marks it started only when its health check passes |
| Main container, after the sidecar is ready | `Requires=`/`After=` the sidecar unit |
| Ordering on another workload's readiness (database before migrations) | The init unit `Requires=`/`After=` the PostgreSQL unit, itself `Notify=healthy` on `pg_isready` |
| Secret volume, `defaultMode`, owner | `Secret=…,type=mount,uid=1654,gid=1654,mode=0400` |
| `emptyDir: {medium: Memory, sizeLimit}` | tmpfs Podman volume (`Type=tmpfs`, `size=1m`); see finding 1 |
| `emptyDir: {sizeLimit}` per container | `Tmpfs=` / `Mount=type=tmpfs,…` |
| `runAsUser/Group`, `readOnlyRootFilesystem`, `capabilities.drop: [ALL]`, `allowPrivilegeEscalation: false`, `RuntimeDefault` seccomp | `User=`, `Group=`, `ReadOnly=`, `DropCapability=ALL`, `NoNewPrivileges=`, Podman's default seccomp |
| `fsGroup` + `fsGroupChangePolicy: OnRootMismatch` | Podman chowns a named volume to the container user on first use only |
| `terminationGracePeriodSeconds`, SIGTERM | `StopSignal=`, `StopTimeout=` |
| `restartPolicy` with crash-loop backoff | `Restart=` with `RestartSec=`, `RestartSteps=`, `RestartMaxDelaySec=` |
| Exec readiness probe on a status file | `HealthCmd=` |

[`scripts/worker.sh`](scripts/worker.sh) prints what a real worker depends on; [`scripts/creds.sh`](scripts/creds.sh)
stands in for a credential provider.

```sh
./run.sh                 # install units for the current user, create secrets, start the worker pod
journalctl --user -u 'rezn-spike*' -f
./cleanup.sh --volumes   # stop and remove everything, including volumes
```

## Results

All checks from #27 passed after the fixes recorded below.

| Check | Result |
| --- | --- |
| Start order | PostgreSQL healthy → init ran → sidecar started; its unit counted as started only after its health check passed, 7 s after the container started → worker started |
| Identity | Worker saw hostname `worker-0` and derived Node `worker-0` |
| User | uid 1654, gid 1654 |
| Files | `/state/data`, `/state/keys`, `/run/creds/worker` 0700; recovery key and token 0600; client secret 0400; all owned 1654:1654 |
| Hardening | Root filesystem read-only; credentials mount read-only in the worker; `CapEff 0`; `NoNewPrivs 1`; `Seccomp 2` (filter) |
| Scratch limits | `/tmp` 64M tmpfs |
| DNS | `pg.spike.svc` resolved from the worker pod; PostgreSQL port reachable |
| Graceful stop | Pod stop sent SIGTERM; worker finished its current item and exited cleanly in 4.1 s (grace 40 s) |
| State retention | After a full pod stop and start: same hostname, same recovery key; volume root kept 1654:1654 without a recursive chown |
| Crash restart | `SIGKILL` to the worker: systemd restarted it in about 3 s; repeated failures backed off 2 s → 4 s → 8 s → 16 s → 30 s |
| Cgroups | Containers run under their unit's cgroup (`--cgroups=split`), so unit resource controls apply to them (not exercised) |

## Findings for Rezn and Orqos

1. **A memory volume lives only while some container uses it.** A Podman tmpfs volume is unmounted, and emptied,
   when no running container mounts it, so the init container's directory under it was gone when the sidecar
   started. Kubernetes' memory `emptyDir` lives as long as the pod. Rezn must give pod-scoped volumes the pod's
   lifetime (for example, keep them mounted by the pod's infra container) or forbid init containers from writing
   to them. Affects [#34](https://github.com/rezn-project/rezn/issues/34) and
   [orqos#21](https://github.com/orqos-project/orqos/issues/21).
2. **`Tmpfs=` takes no `uid`/`gid`.** A per-container tmpfs owned by the workload user needs
   `Mount=type=tmpfs,…,U=true`. The unit renderer must choose the form.
3. **Unit state can drift from real resources.** Deleting the Podman network left its one-shot unit "active", so
   systemd never recreated the network and the pods failed until `reset-failed` and restart. Rezn and Orqos must
   observe actual Podman resources and treat unit state as a hint
   ([orqos#27](https://github.com/orqos-project/orqos/issues/27)).
4. **Restart policies need explicit backoff.** `Restart=always` alone restarted a failing sidecar every 6 s
   indefinitely. Always emit `RestartSec`, `RestartSteps` and `RestartMaxDelaySec` (systemd ≥ 254)
   ([#38](https://github.com/rezn-project/rezn/issues/38)).
5. **`Notify=healthy` gates start, not ongoing readiness.** After start, health changes are not propagated to
   dependants. Readiness-aware service membership needs Rezn or Orqos to watch health status
   ([#59](https://github.com/rezn-project/rezn/issues/59)). Health reads `starting` until the first interval;
   choose start periods and intervals deliberately.
6. **Nothing starts at boot.** These units have no `[Install]` section. Rezn must decide between `WantedBy=` on
   owned units and starting them itself after a reboot
   ([orqos#28](https://github.com/orqos-project/orqos/issues/28)).
7. **Changes are generated, not applied.** Quadlet runs at `daemon-reload`; applying a revision means writing units
   atomically, reloading, and restarting exactly the affected units
   ([orqos#17](https://github.com/orqos-project/orqos/issues/17)).
8. **Volume sizes are not enforced** for named volumes; tmpfs sizes are
   ([#68](https://github.com/rezn-project/rezn/issues/68)).
9. **Rootless maps UIDs into a subordinate range** (container 1654 is host 101653 here). Rootful vs rootless is an
   Orqos decision; rootless worked for every check above.
10. **Noise:** rootless conmon logs `Failed to add inotify watch for /sys/fs/cgroup/memory.events` on every start.

## Not covered

CPU/memory/task limits, network policy, image pulls with registry credentials, rootful mode, host reboot,
several ordinals (Quadlet template units `name@.container` are the obvious mechanism to try for
[#36](https://github.com/rezn-project/rezn/issues/36)), and how much of a rendered chart `podman kube play` runs
([#28](https://github.com/rezn-project/rezn/issues/28)).
