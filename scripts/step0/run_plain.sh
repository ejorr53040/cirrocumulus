#!/usr/bin/env bash
# Boots the Step 0 kernel+rootfs directly with `firecracker` (no jailer),
# drives the Firecracker API over its Unix socket per the upstream quickstart
# (docs/getting-started.md), and waits for the guest's own boot marker on
# its serial console. Prints the console log and exits 0 iff the marker
# appeared before the timeout.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"

FC_BIN="$REPO_ROOT/firecracker"
KERNEL="$(ls "$BUILD_DIR"/vmlinux-* 2>/dev/null | tail -1)"
ROOTFS="$BUILD_DIR/rootfs.ext4"
API_SOCKET="$BUILD_DIR/plain.socket"
CONSOLE_LOG="$BUILD_DIR/plain-console.log"
BOOT_MARKER="STEP0_BOOT_OK"
BOOT_TIMEOUT_S=10

if [ -z "$KERNEL" ] || [ ! -f "$KERNEL" ]; then
    echo "no kernel image found under $BUILD_DIR -- run fetch_kernel.sh first" >&2
    exit 1
fi
if [ ! -f "$ROOTFS" ]; then
    echo "no rootfs image found at $ROOTFS -- run build_rootfs.sh first" >&2
    exit 1
fi

rm -f "$API_SOCKET" "$CONSOLE_LOG"

"$FC_BIN" --api-sock "$API_SOCKET" > "$CONSOLE_LOG" 2>&1 &
FC_PID=$!

cleanup() {
    kill -9 "$FC_PID" >/dev/null 2>&1 || true
    wait "$FC_PID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
    [ -S "$API_SOCKET" ] && break
    sleep 0.1
done
if [ ! -S "$API_SOCKET" ]; then
    echo "firecracker API socket never appeared at $API_SOCKET" >&2
    cat "$CONSOLE_LOG" >&2
    exit 1
fi

api() {
    local method="$1" path="$2" data="$3"
    curl -sS -X "$method" --unix-socket "$API_SOCKET" -d "$data" "http://localhost$path"
}

api PUT /machine-config '{"vcpu_count": 1, "mem_size_mib": 128}' >/dev/null
api PUT /boot-source "{\"kernel_image_path\": \"$KERNEL\", \"boot_args\": \"console=ttyS0 reboot=k panic=1 init=/init\"}" >/dev/null
api PUT /drives/rootfs "{\"drive_id\": \"rootfs\", \"path_on_host\": \"$ROOTFS\", \"is_root_device\": true, \"is_read_only\": false}" >/dev/null
api PUT /actions '{"action_type": "InstanceStart"}' >/dev/null

marker_seen=1
for _ in $(seq 1 $((BOOT_TIMEOUT_S * 10))); do
    if grep -q "$BOOT_MARKER" "$CONSOLE_LOG" 2>/dev/null; then
        marker_seen=0
        break
    fi
    sleep 0.1
done

cat "$CONSOLE_LOG"

exit "$marker_seen"
