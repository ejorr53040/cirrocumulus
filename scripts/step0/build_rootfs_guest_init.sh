#!/usr/bin/env bash
# Builds a rootfs whose /init is the real `guest-init` binary (M2), not
# Step 0's busybox stand-in. Unlike build_rootfs.sh, there's no shell and no
# busybox in this tree at all: guest-init does its own mounting via direct
# syscalls (the `nix` crate), so the kernel can exec it as /init directly.
#
# guest-init is cross-compiled statically for musl so it has zero dynamic
# libc dependency inside the guest -- see RESEARCH.md M2 and
# crates/guest-init.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/guest-init-rootfs-tree"
ROOTFS_IMG="$BUILD_DIR/guest-init-rootfs.ext4"
TARGET="x86_64-unknown-linux-musl"

cargo build --release --target "$TARGET" -p guest-init \
    --manifest-path "$REPO_ROOT/Cargo.toml"

GUEST_INIT_BIN="$REPO_ROOT/target/$TARGET/release/guest-init"

rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"/{proc,sys,dev}

cp "$GUEST_INIT_BIN" "$ROOTFS_TREE/init"
chmod +x "$ROOTFS_TREE/init"

truncate -s 32M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
