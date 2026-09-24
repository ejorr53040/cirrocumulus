#!/usr/bin/env bash
# M2 boot harness: boots the Step 0 kernel with guest-init-rootfs.ext4 (real
# guest-init as /init, not busybox), the same way run_plain.sh boots the
# Step 0 rootfs. Waits for guest-init's own boot marker on the serial
# console. Prints the console log and exits 0 iff the marker appeared
# before the timeout.
#
# Also checks (slice 3) that the guest shuts itself down once the
# configured app exits, rather than needing this script's own cleanup trap
# to kill it: prints synthetic FIRECRACKER_EXITED_CLEANLY / NO_KERNEL_PANIC
# status lines onto its own stdout (alongside the guest's console, which it
# cats below), the same trick run_jailer.sh uses for its own uid check.
# "Firecracker's process exited" alone isn't enough evidence of a *clean*
# shutdown -- slice 1 found that a kernel panic + `reboot=k` also makes
# Firecracker exit with exit_code=0, so NO_KERNEL_PANIC is what actually
# tells clean poweroff apart from panic-triggered auto-reboot.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"

FC_BIN="$REPO_ROOT/firecracker"
KERNEL="$(ls "$BUILD_DIR"/vmlinux-* 2>/dev/null | tail -1)"
ROOTFS="$BUILD_DIR/guest-init-rootfs.ext4"
API_SOCKET="$BUILD_DIR/guest-init.socket"
CONSOLE_LOG="$BUILD_DIR/guest-init-console.log"
BOOT_MARKER="GUEST_INIT_MOUNTS_OK"
BOOT_TIMEOUT_S=10

if [ -z "$KERNEL" ] || [ ! -f "$KERNEL" ]; then
    echo "no kernel image found under $BUILD_DIR -- run fetch_kernel.sh first" >&2
    exit 1
fi
if [ ! -f "$ROOTFS" ]; then
    echo "no rootfs image found at $ROOTFS -- run build_rootfs_guest_init.sh first" >&2
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

# Give the app its own bounded window to appear, then watch what the guest
# does once it exits: FC_PID disappearing on its own (not by our kill -9)
# is the observable signal that guest-init reacted to the app's exit,
# rather than the park-forever fallback from earlier slices.
app_seen=1
for _ in $(seq 1 $((BOOT_TIMEOUT_S * 10))); do
    if grep -q "CHILD_APP_RAN" "$CONSOLE_LOG" 2>/dev/null; then
        app_seen=0
        break
    fi
    sleep 0.1
done

if [ "$app_seen" -eq 0 ]; then
    exited_cleanly=1
    for _ in $(seq 1 50); do
        if ! kill -0 "$FC_PID" >/dev/null 2>&1; then
            exited_cleanly=0
            break
        fi
        sleep 0.1
    done
    if [ "$exited_cleanly" -eq 0 ]; then
        echo "FIRECRACKER_EXITED_CLEANLY: PASS"
    else
        echo "FIRECRACKER_EXITED_CLEANLY: FAIL (still running, needed kill -9)"
    fi
    if grep -q "Kernel panic" "$CONSOLE_LOG" 2>/dev/null; then
        echo "NO_KERNEL_PANIC: FAIL (kernel panicked -- exit_code=0 alone doesn't mean a clean poweroff)"
    else
        echo "NO_KERNEL_PANIC: PASS"
    fi
fi

cat "$CONSOLE_LOG"

exit "$marker_seen"
