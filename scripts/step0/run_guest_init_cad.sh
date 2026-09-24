#!/usr/bin/env bash
# M2 slice 5 boot harness: boots guest-init-rootfs-cad.ext4 (guest-init as
# /init, running long_running_app.rs, which never exits on its own), waits
# for it to start, then calls Firecracker's own `SendCtrlAltDel` action --
# the host-side mechanism this whole slice exists to react to -- instead of
# waiting for the app to exit by itself like run_guest_init.sh's tests do.
# Prints synthetic FIRECRACKER_EXITED_CLEANLY / NO_KERNEL_PANIC status lines
# the same way run_guest_init.sh does, so its assertions are reusable.
#
# Slice 6: the app config travels over vsock port 52 (push_vsock_config.sh),
# not baked into the rootfs -- see build_rootfs_guest_init.sh's note.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"

FC_BIN="$REPO_ROOT/firecracker"
KERNEL="$(ls "$BUILD_DIR"/vmlinux-* 2>/dev/null | tail -1)"
ROOTFS="$BUILD_DIR/guest-init-rootfs-cad.ext4"
API_SOCKET="$BUILD_DIR/guest-init-cad.socket"
CONSOLE_LOG="$BUILD_DIR/guest-init-cad-console.log"
VSOCK_UDS="$BUILD_DIR/guest-init-cad-vsock.sock"
CONFIG_JSON="$BUILD_DIR/cirro-init-cad.json"
BOOT_TIMEOUT_S=10

if [ -z "$KERNEL" ] || [ ! -f "$KERNEL" ]; then
    echo "no kernel image found under $BUILD_DIR -- run fetch_kernel.sh first" >&2
    exit 1
fi
if [ ! -f "$ROOTFS" ]; then
    echo "no rootfs image found at $ROOTFS -- run build_rootfs_guest_init_cad.sh first" >&2
    exit 1
fi
if [ ! -f "$CONFIG_JSON" ]; then
    echo "no config JSON at $CONFIG_JSON -- run build_rootfs_guest_init_cad.sh first" >&2
    exit 1
fi

rm -f "$API_SOCKET" "$CONSOLE_LOG" "$VSOCK_UDS"

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
api PUT /vsock "{\"guest_cid\": 3, \"uds_path\": \"$VSOCK_UDS\"}" >/dev/null
api PUT /actions '{"action_type": "InstanceStart"}' >/dev/null

"$HERE/push_vsock_config.sh" "$VSOCK_UDS" 52 "$CONFIG_JSON" "$BOOT_TIMEOUT_S"

app_seen=1
for _ in $(seq 1 $((BOOT_TIMEOUT_S * 10))); do
    if grep -q "LONG_RUNNING_APP_STARTED" "$CONSOLE_LOG" 2>/dev/null; then
        app_seen=0
        break
    fi
    sleep 0.1
done

if [ "$app_seen" -ne 0 ]; then
    echo "long_running_app never printed its start marker" >&2
    cat "$CONSOLE_LOG"
    exit 1
fi

# The app is up and would park forever on its own -- this is the host
# action the whole slice is about: ask Firecracker to deliver a graceful
# shutdown request into the guest, the same way a real operator's "stop"
# would.
api PUT /actions '{"action_type": "SendCtrlAltDel"}' >/dev/null

exited_cleanly=1
for _ in $(seq 1 $((BOOT_TIMEOUT_S * 10))); do
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

cat "$CONSOLE_LOG"

exit 0
