#!/usr/bin/env bash
# Pushes a JSON config to guest-init over vsock port 52 (RESEARCH.md M2's
# vsock config channel, replacing the fixed-path /etc/cirro-init.json from
# slices 2-5), using Firecracker's host-initiated vsock protocol: connect
# to the vsock UDS Firecracker exposes on the host, send "CONNECT
# <port>\n", and everything written after that is relayed straight into
# whatever the guest has accepted on that port. Firecracker acks with "OK
# <port>\n" first; closing this end (EOF) is how guest-init's read loop
# knows the config is complete.
#
# guest-init's vsock listener isn't bound until it's already mounted
# filesystems and installed its signal handler, a handful of instructions
# after the boot marker a caller might have just waited on -- so this
# retries the whole handshake (a fresh connection each time; a rejected
# CONNECT before the guest is listening never reaches guest-init at all,
# so retrying from scratch is safe) rather than assuming one attempt lands.
set -uo pipefail

UDS_PATH="$1"
PORT="$2"
CONFIG_JSON_FILE="$3"
TIMEOUT_S="${4:-10}"

for _ in $(seq 1 $((TIMEOUT_S * 10))); do
    response="$( { printf 'CONNECT %s\n' "$PORT"; cat "$CONFIG_JSON_FILE"; } \
        | timeout 1 socat - "UNIX-CONNECT:$UDS_PATH" 2>/dev/null )"
    if printf '%s' "$response" | grep -q "^OK"; then
        exit 0
    fi
    sleep 0.1
done

echo "push_vsock_config.sh: never got an OK from $UDS_PATH port $PORT within ${TIMEOUT_S}s" >&2
exit 1
