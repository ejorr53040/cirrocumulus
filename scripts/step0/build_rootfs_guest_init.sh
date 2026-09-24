#!/usr/bin/env bash
# Builds a rootfs whose /init is the real `guest-init` binary (M2), not
# Step 0's busybox stand-in. Unlike build_rootfs.sh, there's no shell and no
# busybox in this tree at all: guest-init does its own mounting via direct
# syscalls (the `nix` crate), so the kernel can exec it as /init directly.
#
# guest-init is cross-compiled statically for musl so it has zero dynamic
# libc dependency inside the guest -- see RESEARCH.md M2 and
# crates/guest-init.
#
# Also stages the M2 slice 2 fixtures: a config file at the fixed path
# guest-init reads until vsock config exists, and a tiny static "app"
# binary for it to fork+exec (fixtures/child_app.rs -- not part of the
# cargo workspace, since it's a boot-test fixture, not shipped code).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/guest-init-rootfs-tree"
ROOTFS_IMG="$BUILD_DIR/guest-init-rootfs.ext4"
TARGET="x86_64-unknown-linux-musl"
CHILD_APP_SRC="$HERE/fixtures/child_app.rs"
CHILD_APP_BIN="$BUILD_DIR/child_app"

cargo build --release --target "$TARGET" -p guest-init \
    --manifest-path "$REPO_ROOT/Cargo.toml"

GUEST_INIT_BIN="$REPO_ROOT/target/$TARGET/release/guest-init"

rustc --target "$TARGET" -O -o "$CHILD_APP_BIN" "$CHILD_APP_SRC"

rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"/{proc,sys,dev,etc,app}

cp "$GUEST_INIT_BIN" "$ROOTFS_TREE/init"
chmod +x "$ROOTFS_TREE/init"

cp "$CHILD_APP_BIN" "$ROOTFS_TREE/app/child_app"
chmod +x "$ROOTFS_TREE/app/child_app"

cat > "$ROOTFS_TREE/etc/cirro-init.json" <<'EOF'
{"exec": "/app/child_app", "args": []}
EOF

truncate -s 32M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
