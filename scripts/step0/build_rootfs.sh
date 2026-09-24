#!/usr/bin/env bash
# Builds the smallest possible bootable rootfs: static busybox as PID 1,
# mounts proc/sys/devtmpfs, prints a marker line to the console so the boot
# scripts can confirm userspace was reached, then powers off cleanly.
#
# This deliberately isn't the design doc's future Rust guest-init (that's
# Step 2) -- it exists only to prove Step 0's question: can this machine
# actually boot a Firecracker microVM to userspace, under jailer, at all.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/rootfs-tree"
ROOTFS_IMG="$BUILD_DIR/rootfs.ext4"
BOOT_MARKER="STEP0_BOOT_OK"

BUSYBOX_BIN="$(command -v busybox)"

rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"/{bin,sbin,proc,sys,dev,etc,usr/bin,usr/sbin}

cp "$BUSYBOX_BIN" "$ROOTFS_TREE/bin/busybox"

# Symlink every busybox applet (sh, mount, poweroff, ...) into /bin. Skip
# "busybox" itself -- it's in --list too, and symlinking it over the real
# binary makes it point at itself (ELOOP: exec fails with error -40).
for applet in $("$BUSYBOX_BIN" --list); do
    [ "$applet" = "busybox" ] && continue
    ln -sf busybox "$ROOTFS_TREE/bin/$applet"
done

# Booted directly via the kernel's init=/init arg, not busybox's own init
# applet (which would expect /etc/inittab).
cat > "$ROOTFS_TREE/init" <<EOF
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
echo "$BOOT_MARKER"
poweroff -f
EOF
chmod +x "$ROOTFS_TREE/init"

truncate -s 32M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
