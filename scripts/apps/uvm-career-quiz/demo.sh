#!/usr/bin/env bash
# End-to-end proof that cirrocumulus can host a real app end to end:
#
#   1. boot UVM-Career-Quiz (github.com/VERSO-UVM/UVM-Career-Quiz) in a
#      Firecracker microVM, guest-init (crates/guest-init) as /init,
#   2. reach it over real TCP/IP (login page, then log in and load an
#      authenticated page), proving HTTP actually round-trips through the
#      guest's network stack, not just the console,
#   3. ask Firecracker to shut it down (`SendCtrlAltDel`) and confirm
#      guest-init forwards that as a real SIGTERM (RESEARCH.md M2 slice 5)
#      and the VM exits cleanly, not via a hard reset or a hung process.
#
# The app config (which binary to exec) travels over vsock port 52 at
# boot (M2 slice 6), pushed via scripts/step0/push_vsock_config.sh from
# the JSON build_rootfs.sh writes to .build/cirro-init.json -- it's no
# longer baked into the image.
#
# No root needed: run inside an unprivileged net+user namespace, which
# grants CAP_NET_ADMIN for creating the tap device without touching the
# real host at all (verified by hand: `unshare --net --user
# --map-root-user -- ip tuntap add dev t0 mode tap` succeeds where a plain
# `ip tuntap add` as a regular user fails with "Operation not permitted").
#
#   unshare --net --user --map-root-user -- ./demo.sh
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
BUILD_DIR="$HERE/.build"

FC_BIN="$REPO_ROOT/firecracker"
KERNEL="$(ls "$REPO_ROOT"/scripts/step0/.build/vmlinux-* 2>/dev/null | tail -1)"
ROOTFS="$BUILD_DIR/uvm-career-quiz.ext4"
API_SOCKET="$BUILD_DIR/uvm-career-quiz.socket"
CONSOLE_LOG="$BUILD_DIR/uvm-career-quiz-console.log"
VSOCK_UDS="$BUILD_DIR/uvm-career-quiz-vsock.sock"
CONFIG_JSON="$BUILD_DIR/cirro-init.json"
PUSH_VSOCK_CONFIG="$REPO_ROOT/scripts/step0/push_vsock_config.sh"

TAP_DEV="tap0"
HOST_IP="172.16.0.1"
GUEST_IP="172.16.0.2"
PREFIX=30
BOOT_TIMEOUT_S=15

if [ "$(id -u)" -ne 0 ]; then
    echo "not running with CAP_NET_ADMIN -- re-run as:" >&2
    echo "  unshare --net --user --map-root-user -- $0" >&2
    exit 1
fi
if [ -z "$KERNEL" ] || [ ! -f "$KERNEL" ]; then
    echo "no kernel image found -- run scripts/step0/fetch_kernel.sh first" >&2
    exit 1
fi
if [ ! -f "$ROOTFS" ]; then
    echo "no rootfs at $ROOTFS -- run fetch_app.sh then build_rootfs.sh first" >&2
    exit 1
fi
if [ ! -f "$CONFIG_JSON" ]; then
    echo "no config JSON at $CONFIG_JSON -- run build_rootfs.sh first" >&2
    exit 1
fi

echo "==> setting up tap networking ($TAP_DEV, $HOST_IP/$PREFIX <-> guest $GUEST_IP)"
ip tuntap add dev "$TAP_DEV" mode tap
ip addr add "$HOST_IP/$PREFIX" dev "$TAP_DEV"
ip link set "$TAP_DEV" up

rm -f "$API_SOCKET" "$CONSOLE_LOG" "$VSOCK_UDS"
"$FC_BIN" --api-sock "$API_SOCKET" > "$CONSOLE_LOG" 2>&1 &
FC_PID=$!

cleanup() {
    kill -9 "$FC_PID" >/dev/null 2>&1 || true
    wait "$FC_PID" 2>/dev/null || true
    ip link del "$TAP_DEV" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
    [ -S "$API_SOCKET" ] && break
    sleep 0.1
done
if [ ! -S "$API_SOCKET" ]; then
    echo "firecracker API socket never appeared" >&2
    cat "$CONSOLE_LOG" >&2
    exit 1
fi

api() {
    local method="$1" path="$2" data="$3"
    curl -sS -X "$method" --unix-socket "$API_SOCKET" -d "$data" "http://localhost$path"
}

echo "==> booting"
api PUT /machine-config '{"vcpu_count": 1, "mem_size_mib": 256}' >/dev/null
api PUT /boot-source "{\"kernel_image_path\": \"$KERNEL\", \"boot_args\": \"console=ttyS0 reboot=k panic=1 init=/init\"}" >/dev/null
api PUT /drives/rootfs "{\"drive_id\": \"rootfs\", \"path_on_host\": \"$ROOTFS\", \"is_root_device\": true, \"is_read_only\": false}" >/dev/null
api PUT "/network-interfaces/eth0" "{\"iface_id\": \"eth0\", \"host_dev_name\": \"$TAP_DEV\"}" >/dev/null
api PUT /vsock "{\"guest_cid\": 3, \"uds_path\": \"$VSOCK_UDS\"}" >/dev/null
api PUT /actions '{"action_type": "InstanceStart"}' >/dev/null

echo "==> pushing app config over vsock"
"$PUSH_VSOCK_CONFIG" "$VSOCK_UDS" 52 "$CONFIG_JSON" "$BOOT_TIMEOUT_S"

marker_seen=1
for _ in $(seq 1 $((BOOT_TIMEOUT_S * 10))); do
    if grep -q "GUEST_INIT_MOUNTS_OK" "$CONSOLE_LOG" 2>/dev/null; then
        marker_seen=0
        break
    fi
    sleep 0.1
done
if [ "$marker_seen" -ne 0 ]; then
    echo "guest-init never reached userspace" >&2
    cat "$CONSOLE_LOG" >&2
    exit 1
fi
echo "==> guest-init up, waiting for the app to bind its port"

app_up=1
for _ in $(seq 1 $((BOOT_TIMEOUT_S * 10))); do
    if curl -sS --max-time 1 -o /dev/null "http://$GUEST_IP:5000/" 2>/dev/null; then
        app_up=0
        break
    fi
    sleep 0.2
done
if [ "$app_up" -ne 0 ]; then
    echo "HTTP_REACHABLE: FAIL (app never answered on $GUEST_IP:5000)" >&2
    cat "$CONSOLE_LOG" >&2
    exit 1
fi
echo "HTTP_REACHABLE: PASS"

echo "==> GET / (login page)"
login_status="$(curl -sS -c "$BUILD_DIR/cookies.txt" -o "$BUILD_DIR/login_page.html" -w '%{http_code}' "http://$GUEST_IP:5000/")"
echo "GET / -> HTTP $login_status"
grep -q "Welcome Page\|login" "$BUILD_DIR/login_page.html" && echo "LOGIN_PAGE_RENDERED: PASS" || echo "LOGIN_PAGE_RENDERED: FAIL"

echo "==> POST /quiz_login (seeded user 1/1 from database_interaction.py's create_db())"
login_redirect_status="$(curl -sS -b "$BUILD_DIR/cookies.txt" -c "$BUILD_DIR/cookies.txt" -o /dev/null -w '%{http_code}' \
    -d "username=1&password=1" "http://$GUEST_IP:5000/quiz_login")"
echo "POST /quiz_login -> HTTP $login_redirect_status"

echo "==> GET /quiz_selection (authenticated, using the session cookie from login)"
selection_status="$(curl -sS -b "$BUILD_DIR/cookies.txt" -o "$BUILD_DIR/quiz_selection.html" -w '%{http_code}' "http://$GUEST_IP:5000/quiz_selection")"
echo "GET /quiz_selection -> HTTP $selection_status"
if grep -q "testing quiz" "$BUILD_DIR/quiz_selection.html"; then
    echo "AUTHENTICATED_ROUND_TRIP: PASS (saw the seeded quiz name)"
else
    echo "AUTHENTICATED_ROUND_TRIP: FAIL"
fi

echo "==> requesting shutdown (Firecracker SendCtrlAltDel)"
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
    echo "NO_KERNEL_PANIC: FAIL"
else
    echo "NO_KERNEL_PANIC: PASS"
fi
if grep -q "GUEST_INIT_FORWARDING_SIGTERM" "$CONSOLE_LOG" 2>/dev/null; then
    echo "SIGTERM_FORWARDED_TO_APP: PASS"
else
    echo "SIGTERM_FORWARDED_TO_APP: FAIL"
fi

echo "==> full console log:"
cat "$CONSOLE_LOG"
