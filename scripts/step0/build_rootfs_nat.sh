#!/usr/bin/env bash
# A rootfs variant for the NAT test (RESEARCH.md M3, nftables NAT slice):
# unlike build_rootfs.sh's plain rootfs (which only checks eth0 is
# *present*, never configures it), this guest actively configures its own
# address and default route on eth0, then probes real outbound
# connectivity through it -- the only way to actually prove the host's
# NAT/masquerade rule works, versus just asserting the rule exists.
#
# Addressing is hardcoded to match the host side the NAT test itself
# configures (172.16.61.1/30 host, .2 guest, gateway .1) -- this rootfs is
# only ever booted by that one test, so there's no config-passing seam to
# build yet (that's M2 slice 6's vsock config, not reused here since this
# is plain busybox, not guest-init).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/rootfs-nat-tree"
ROOTFS_IMG="$BUILD_DIR/rootfs-nat.ext4"
BOOT_MARKER="STEP0_BOOT_OK"
INTERNET_MARKER="STEP0_HAS_INTERNET"

GUEST_IP="172.16.61.2"
GUEST_PREFIX="30"
GATEWAY_IP="172.16.61.1"
# A fixed, well-known public IP -- avoids depending on the guest having
# working DNS (it doesn't configure any resolver), and `nc -z` only needs
# a TCP handshake to prove the NAT'd path works end to end, not a full
# HTTP response.
PROBE_IP="1.1.1.1"
PROBE_PORT="443"

BUSYBOX_BIN="$(command -v busybox)"

rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"/{bin,sbin,proc,sys,dev,etc,usr/bin,usr/sbin}

cp "$BUSYBOX_BIN" "$ROOTFS_TREE/bin/busybox"

for applet in $("$BUSYBOX_BIN" --list); do
    [ "$applet" = "busybox" ] && continue
    ln -sf busybox "$ROOTFS_TREE/bin/$applet"
done

cat > "$ROOTFS_TREE/init" <<EOF
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
echo "$BOOT_MARKER"
ip addr add $GUEST_IP/$GUEST_PREFIX dev eth0 && echo "GUEST_ADDR_SET"
ip link set eth0 up && echo "GUEST_LINK_UP"
ip route add default via $GATEWAY_IP && echo "GUEST_ROUTE_SET"
ip addr show eth0
ip route show
nc -zw3 $PROBE_IP $PROBE_PORT
echo "GUEST_NC_EXIT=\$?"
nc -zw3 $PROBE_IP $PROBE_PORT && echo "$INTERNET_MARKER"
poweroff -f
EOF
chmod +x "$ROOTFS_TREE/init"

truncate -s 32M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
