#!/usr/bin/env bash
# M2 slice 2 speed test: how long from "start this VM" to "the configured
# app is running" -- the real number a request-triggered wake would block
# on. This is guest-init's current floor (mount + fork/exec, no snapshot
# restore yet); M6 (park/wake with snapshots) is what's meant to beat it.
#
# Methodology: for each of N runs, boot fresh (new firecracker process, new
# API socket), time from the instant the InstanceStart PUT is issued to the
# instant CHILD_APP_RAN first appears in the console log, polling every 1ms
# (not the 100ms granularity the correctness tests use -- too coarse to say
# anything meaningful about a boot this fast). Prints one line per run plus
# a summary; --json emits machine-readable output for another script to
# consume (used here to render the markdown report).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"

FC_BIN="$REPO_ROOT/firecracker"
KERNEL="$(ls "$BUILD_DIR"/vmlinux-* 2>/dev/null | tail -1)"
ROOTFS="$BUILD_DIR/guest-init-rootfs.ext4"
BOOT_MARKER="CHILD_APP_RAN"
POLL_TIMEOUT_S=10
RUNS="${1:-20}"

if [ -z "$KERNEL" ] || [ ! -f "$KERNEL" ]; then
    echo "no kernel image found under $BUILD_DIR -- run fetch_kernel.sh first" >&2
    exit 1
fi
if [ ! -f "$ROOTFS" ]; then
    echo "no rootfs image found at $ROOTFS -- run build_rootfs_guest_init.sh first" >&2
    exit 1
fi

api() {
    local socket="$1" method="$2" path="$3" data="$4"
    curl -sS -X "$method" --unix-socket "$socket" -d "$data" "http://localhost$path"
}

declare -a TIMES_MS=()

for i in $(seq 1 "$RUNS"); do
    API_SOCKET="$BUILD_DIR/speedtest.socket"
    CONSOLE_LOG="$BUILD_DIR/speedtest-console.log"
    rm -f "$API_SOCKET" "$CONSOLE_LOG"

    "$FC_BIN" --api-sock "$API_SOCKET" > "$CONSOLE_LOG" 2>&1 &
    FC_PID=$!

    for _ in $(seq 1 200); do
        [ -S "$API_SOCKET" ] && break
        sleep 0.005
    done
    if [ ! -S "$API_SOCKET" ]; then
        echo "run $i: firecracker API socket never appeared" >&2
        kill -9 "$FC_PID" >/dev/null 2>&1 || true
        wait "$FC_PID" 2>/dev/null || true
        continue
    fi

    api "$API_SOCKET" PUT /machine-config '{"vcpu_count": 1, "mem_size_mib": 128}' >/dev/null
    api "$API_SOCKET" PUT /boot-source "{\"kernel_image_path\": \"$KERNEL\", \"boot_args\": \"console=ttyS0 reboot=k panic=1 init=/init\"}" >/dev/null
    api "$API_SOCKET" PUT /drives/rootfs "{\"drive_id\": \"rootfs\", \"path_on_host\": \"$ROOTFS\", \"is_root_device\": true, \"is_read_only\": false}" >/dev/null

    t0=$(date +%s.%N)
    api "$API_SOCKET" PUT /actions '{"action_type": "InstanceStart"}' >/dev/null

    marker_seen=1
    for _ in $(seq 1 $((POLL_TIMEOUT_S * 1000))); do
        if grep -q "$BOOT_MARKER" "$CONSOLE_LOG" 2>/dev/null; then
            marker_seen=0
            break
        fi
        sleep 0.001
    done
    t1=$(date +%s.%N)

    kill -9 "$FC_PID" >/dev/null 2>&1 || true
    wait "$FC_PID" 2>/dev/null || true

    if [ "$marker_seen" -ne 0 ]; then
        echo "run $i: marker never appeared within ${POLL_TIMEOUT_S}s" >&2
        continue
    fi

    ms=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", (b - a) * 1000 }')
    TIMES_MS+=("$ms")
    echo "run $i: ${ms} ms"
done

rm -f "$BUILD_DIR/speedtest.socket" "$BUILD_DIR/speedtest-console.log"

if [ "${#TIMES_MS[@]}" -eq 0 ]; then
    echo "no successful runs" >&2
    exit 1
fi

python3 - "${TIMES_MS[@]}" <<'PYEOF'
import sys, statistics

times = [float(x) for x in sys.argv[1:]]
print()
print(f"n       = {len(times)}")
print(f"min     = {min(times):.3f} ms")
print(f"max     = {max(times):.3f} ms")
print(f"mean    = {statistics.mean(times):.3f} ms")
print(f"median  = {statistics.median(times):.3f} ms")
if len(times) > 1:
    print(f"stdev   = {statistics.stdev(times):.3f} ms")
PYEOF
