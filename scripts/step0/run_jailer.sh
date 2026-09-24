#!/usr/bin/env bash
# Boots the Step 0 kernel+rootfs under `jailer` (chroot + cgroup + privilege
# drop), per docs/jailer.md. jailer itself must run as root (passwordless
# sudo scoped to this exact binary is set up as a one-time prerequisite --
# see prereqs.sh); firecracker inside the jail runs as an unprivileged uid.
#
# Mirrors run_plain.sh's API sequence, but paths given to the Firecracker
# API must be paths as seen *inside* the chroot, and the kernel/rootfs files
# must be copied into the jail root before those API calls are made.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"

JAILER_BIN="$REPO_ROOT/jailer"
FC_BIN="$REPO_ROOT/firecracker"
KERNEL_SRC="$(ls "$BUILD_DIR"/vmlinux-* 2>/dev/null | tail -1)"
ROOTFS_SRC="$BUILD_DIR/rootfs.ext4"
# jailer (running as root) leaves the two directories above the jail root
# ("<chroot-base>/firecracker/<id>/") root-owned -- only the leaf "root/"
# dir gets chowned to our uid. That means we can't clean those up between
# runs without sudo, so each run gets its own id and old ones are left
# behind (harmless, small, and /tmp gets reaped anyway).
#
# CHROOT_BASE must also be short: the Firecracker API socket path
# (<chroot-base>/firecracker/<id>/root/run/firecracker.socket) has to fit
# in a sockaddr_un, which curl caps at 108 bytes. And it can't be on a
# `nodev`-mounted filesystem (e.g. /tmp here) -- jailer mknods /dev/kvm
# inside the chroot, and device nodes are inert on a nodev mount, which
# surfaces as a confusing "Permission denied" from Firecracker's own KVM
# init rather than any obvious mount-related error.
CHROOT_BASE="$HOME/.cirrocumulus-step0-jail"
CONSOLE_LOG="$BUILD_DIR/jailer-console.log"
BOOT_MARKER="STEP0_BOOT_OK"
BOOT_TIMEOUT_S=10

JAIL_ID="s0-$$"
JAIL_UID="$(id -u)"
JAIL_GID="$(id -g)"

if [ -z "$KERNEL_SRC" ] || [ ! -f "$KERNEL_SRC" ]; then
    echo "no kernel image found under $BUILD_DIR -- run fetch_kernel.sh first" >&2
    exit 1
fi
if [ ! -f "$ROOTFS_SRC" ]; then
    echo "no rootfs image found at $ROOTFS_SRC -- run build_rootfs.sh first" >&2
    exit 1
fi

JAIL_ROOT="$CHROOT_BASE/$(basename "$FC_BIN")/$JAIL_ID/root"
rm -f "$CONSOLE_LOG"
mkdir -p "$CHROOT_BASE"

sudo "$JAILER_BIN" \
    --id "$JAIL_ID" \
    --exec-file "$FC_BIN" \
    --uid "$JAIL_UID" \
    --gid "$JAIL_GID" \
    --chroot-base-dir "$CHROOT_BASE" \
    --cgroup-version 2 \
    > "$CONSOLE_LOG" 2>&1 &
JAILER_PID=$!

cleanup() {
    # jailer drops privileges to $JAIL_UID before exec'ing firecracker, so
    # once it's running we can kill it directly -- no sudo needed (and our
    # NOPASSWD rule only covers invoking jailer itself, not killing it).
    # The parent `sudo jailer` process ($JAILER_PID) normally exits on its
    # own once its child does; if it doesn't, it's a harmless orphaned
    # process, not one this script can clean up without a password.
    if [ -f "$JAIL_ROOT/firecracker.pid" ]; then
        kill -9 "$(cat "$JAIL_ROOT/firecracker.pid")" >/dev/null 2>&1 || true
    fi
    wait "$JAILER_PID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
    [ -d "$JAIL_ROOT" ] && break
    sleep 0.1
done
if [ ! -d "$JAIL_ROOT" ]; then
    echo "jailer never created $JAIL_ROOT" >&2
    cat "$CONSOLE_LOG" >&2
    exit 1
fi

# Resources must be placed inside the jail root before they're referenced
# over the API (docs/jailer.md: "create hard links for (or copy) any
# resources ... inside the jailed root folder").
cp "$KERNEL_SRC" "$JAIL_ROOT/$(basename "$KERNEL_SRC")"
cp "$ROOTFS_SRC" "$JAIL_ROOT/rootfs.ext4"

API_SOCKET="$JAIL_ROOT/run/firecracker.socket"
for _ in $(seq 1 50); do
    [ -S "$API_SOCKET" ] && break
    sleep 0.1
done
if [ ! -S "$API_SOCKET" ]; then
    echo "firecracker API socket never appeared at $API_SOCKET" >&2
    cat "$CONSOLE_LOG" >&2
    exit 1
fi

# The socket file's owning uid reveals who's actually serving the API --
# jailer's whole job is to make sure that's not root.
socket_uid="$(stat -c %u "$API_SOCKET")"
if [ "$socket_uid" != "0" ]; then
    echo "firecracker not running as root: PASS (socket owned by uid $socket_uid)"
else
    echo "firecracker not running as root: FAIL (socket owned by root)"
fi

api() {
    local method="$1" path="$2" data="$3"
    curl -sS -X "$method" --unix-socket "$API_SOCKET" -d "$data" "http://localhost$path"
}

api PUT /machine-config '{"vcpu_count": 1, "mem_size_mib": 128}' >/dev/null
api PUT /boot-source "{\"kernel_image_path\": \"/$(basename "$KERNEL_SRC")\", \"boot_args\": \"console=ttyS0 reboot=k panic=1 init=/init\"}" >/dev/null
api PUT /drives/rootfs '{"drive_id": "rootfs", "path_on_host": "/rootfs.ext4", "is_root_device": true, "is_read_only": false}' >/dev/null
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
