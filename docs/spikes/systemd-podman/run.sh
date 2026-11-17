#!/bin/sh
# Install the spike's Quadlet units for the current user and start the worker pod (which pulls in PostgreSQL).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dest="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/rezn-spike"
mkdir -p "$dest"
for f in "$here"/units/*; do sed "s#@SPIKE_DIR@#$here#g" "$f" > "$dest/$(basename "$f")"; done
podman secret exists rezn-spike-pg-password || head -c 24 /dev/urandom | base64 | podman secret create rezn-spike-pg-password - >/dev/null
podman secret exists rezn-spike-worker-clients || head -c 24 /dev/urandom | base64 | podman secret create rezn-spike-worker-clients - >/dev/null
/usr/libexec/podman/quadlet -dryrun -user >/dev/null
systemctl --user daemon-reload
systemctl --user start rezn-spike-worker-pod.service
