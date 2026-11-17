#!/bin/sh
# Stop and remove everything the spike created; pass --volumes to delete its volumes too.
set -u
dest="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/rezn-spike"
systemctl --user stop 'rezn-spike*' 2>/dev/null
systemctl --user reset-failed 'rezn-spike*' 2>/dev/null
rm -rf "$dest"
systemctl --user daemon-reload
podman secret rm rezn-spike-pg-password rezn-spike-worker-clients 2>/dev/null
podman network rm rezn-spike 2>/dev/null
[ "${1:-}" = --volumes ] && podman volume rm rezn-spike-pg-0 rezn-spike-worker-0 rezn-spike-worker-0-creds 2>/dev/null
true
