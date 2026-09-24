#!/usr/bin/env bash
# M2 slice 5 seam: a second guest-init rootfs, separate from
# guest-init-rootfs.ext4, whose configured app (fixtures/long_running_app.rs)
# never exits on its own -- every M2 slice 2-4 test needs child_app, which
# exits promptly, so a park-forever app needs its own image with its own
# app binary staged at a different path, rather than overloading that one.
#
# The config JSON itself (which binary to exec) travels over vsock at
# boot (slice 6), not baked into this image -- see
# build_rootfs_guest_init.sh's own note on why.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/guest-init-rootfs-cad-tree"
ROOTFS_IMG="$BUILD_DIR/guest-init-rootfs-cad.ext4"
TARGET="x86_64-unknown-linux-musl"
APP_SRC="$HERE/fixtures/long_running_app.rs"
APP_BIN="$BUILD_DIR/long_running_app"

cargo build --release --target "$TARGET" -p guest-init \
    --manifest-path "$REPO_ROOT/Cargo.toml"

GUEST_INIT_BIN="$REPO_ROOT/target/$TARGET/release/guest-init"

rustc --target "$TARGET" -O -o "$APP_BIN" "$APP_SRC"

rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"/{proc,sys,dev,etc,app}

cp "$GUEST_INIT_BIN" "$ROOTFS_TREE/init"
chmod +x "$ROOTFS_TREE/init"

cp "$APP_BIN" "$ROOTFS_TREE/app/long_running_app"
chmod +x "$ROOTFS_TREE/app/long_running_app"

cat > "$BUILD_DIR/cirro-init-cad.json" <<'EOF'
{"exec": "/app/long_running_app", "args": []}
EOF

truncate -s 32M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
