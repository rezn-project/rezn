#!/bin/sh
# Stand-in worker: reports what a real worker relies on, then processes items until SIGTERM.
set -u
host=$(hostname); ordinal=${host##*-}
echo "worker: hostname=$host node=worker-$ordinal uid=$(id -u) gid=$(id -g)"
for f in /state/data /state/keys /state/keys/recovery.key /run/creds/worker /run/creds/worker/access-token /run/client/client-secret-$ordinal; do
  echo "worker: $(stat -c '%n owner=%u:%g mode=%a' "$f")"
done
if touch /rootfs-probe 2>/dev/null; then echo "worker: root filesystem WRITABLE"; else echo "worker: root filesystem read-only"; fi
if touch /run/creds/worker/probe 2>/dev/null; then echo "worker: credentials WRITABLE"; else echo "worker: credentials read-only"; fi
echo "worker: $(grep -E '^(CapEff|NoNewPrivs|Seccomp):' /proc/self/status | tr '\t\n' '  ')"
echo "worker: /tmp $(df -h /tmp | awk 'NR==2{print $2}')"
echo "worker: pg.spike.svc -> $(getent hosts pg.spike.svc | awk '{print $1}')"
if nc -z -w 3 pg.spike.svc 5432; then echo "worker: PostgreSQL reachable"; else echo "worker: PostgreSQL NOT reachable"; fi
stopping=0
trap 'echo "worker: SIGTERM, finishing current item"; stopping=1' TERM
i=0
while [ "$stopping" -eq 0 ]; do
  i=$((i + 1))
  printf '{"state":"running","item":%d}\n' "$i" > /run/status/worker.json
  sleep 5 & wait $!
done
sleep 3
echo "worker: item $i finished, exiting cleanly"
