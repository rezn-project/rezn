#!/bin/sh
# Stand-in for a credential provider: derives its client from the hostname ordinal, writes owner-only files.
set -eu
umask 077
ordinal=$(hostname); ordinal=${ordinal##*-}
secret=/run/client/client-secret-$ordinal
test -s "$secret"
mkdir -p /run/creds/worker; chmod 0700 /run/creds/worker
echo "creds: client worker-$ordinal, secret $(stat -c 'owner=%u:%g mode=%a' "$secret")"
sleep 5   # slow start, to show the worker waits for health
[ -s /state/keys/recovery.key ] || head -c 32 /dev/urandom | base64 > /state/keys/recovery.key
refresh() { echo "token-worker-$ordinal-$(date +%s)" > /run/creds/worker/access-token.tmp; mv /run/creds/worker/access-token.tmp /run/creds/worker/access-token; }
refresh
echo "creds: token and recovery key written"
trap 'echo "creds: stopping"; exit 0' TERM
while :; do sleep 30 & wait $!; refresh; done
