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
# Also stages the M2 slice 2 fixture app guest-init forks+execs
# (fixtures/child_app.rs -- not part of the cargo workspace, since it's a
# boot-test fixture, not shipped code). Slice 4 adds a second fixture,
# fixtures/grandchild.rs, which child_app spawns and never waits on, so
# guest-init's reap loop has an orphan to actually reap.
#
# The app's config (which binary to exec) is no longer baked into the
# image: since slice 6, guest-init reads it over vsock at boot instead of
# from a fixed rootfs path, so it's the *runner*'s job (run_guest_init.sh,
# via push_vsock_config.sh) to supply it, not this build step's. This
# script still writes the JSON, just outside the rootfs tree, as the file
# run_guest_init.sh pushes.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/guest-init-rootfs-tree"
ROOTFS_IMG="$BUILD_DIR/guest-init-rootfs.ext4"
TARGET="x86_64-unknown-linux-musl"
CHILD_APP_SRC="$HERE/fixtures/child_app.rs"
CHILD_APP_BIN="$BUILD_DIR/child_app"
GRANDCHILD_SRC="$HERE/fixtures/grandchild.rs"
GRANDCHILD_BIN="$BUILD_DIR/grandchild"

cargo build --release --target "$TARGET" -p guest-init \
    --manifest-path "$REPO_ROOT/Cargo.toml"

GUEST_INIT_BIN="$REPO_ROOT/target/$TARGET/release/guest-init"

rustc --target "$TARGET" -O -o "$CHILD_APP_BIN" "$CHILD_APP_SRC"
rustc --target "$TARGET" -O -o "$GRANDCHILD_BIN" "$GRANDCHILD_SRC"

rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"/{proc,sys,dev,etc,app}

cp "$GUEST_INIT_BIN" "$ROOTFS_TREE/init"
chmod +x "$ROOTFS_TREE/init"

cp "$CHILD_APP_BIN" "$ROOTFS_TREE/app/child_app"
chmod +x "$ROOTFS_TREE/app/child_app"

cp "$GRANDCHILD_BIN" "$ROOTFS_TREE/app/grandchild"
chmod +x "$ROOTFS_TREE/app/grandchild"

cat > "$BUILD_DIR/cirro-init.json" <<'EOF'
{"exec": "/app/child_app", "args": []}
EOF

truncate -s 32M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
