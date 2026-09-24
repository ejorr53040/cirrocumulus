#!/usr/bin/env bash
# Builds a rootfs that boots the real guest-init (crates/guest-init, M2) as
# /init and runs UVM-Career-Quiz's Flask app as the configured app.
#
# No Docker, no root: Alpine's `apk.static` (a fully static binary -- see
# https://pkgs.alpinelinux.org/package/edge/main/x86_64/apk-tools-static)
# can install packages into an arbitrary --root directory as a plain file
# operation, without chrooting or needing CAP_SYS_ADMIN/CAP_CHOWN. Its
# per-package post-install/trigger scripts (busybox's symlink-farm
# installer, `chown`/`setuid`-bit bookkeeping) do need root and fail here
# -- expected and harmless for this single-process-as-PID-1 guest, which
# never runs as a non-root user and calls busybox applets directly
# (`/bin/busybox ip ...`) rather than through symlinks. Verified once by
# hand: `unshare --user --map-root-user --mount -- chroot <rootfs>
# /usr/bin/python3 -c 'import flask, sqlite3'` succeeds against this same
# install.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
BUILD_DIR="$HERE/.build"
ROOTFS_TREE="$BUILD_DIR/rootfs"
ROOTFS_IMG="$BUILD_DIR/uvm-career-quiz.ext4"
APP_SRC="$BUILD_DIR/app-src"
TARGET="x86_64-unknown-linux-musl"

ALPINE_VERSION="3.20.3"
ALPINE_BRANCH="v3.20"
APK_TOOLS_STATIC_PKG="apk-tools-static-2.14.4-r1.apk"

mkdir -p "$BUILD_DIR"

if [ ! -d "$APP_SRC/.git" ]; then
    echo "no app source at $APP_SRC -- run fetch_app.sh first" >&2
    exit 1
fi

# --- fetch Alpine's static apk tool (idempotent) ---------------------------
APK_DIR="$BUILD_DIR/apk-tools-static"
APK="$APK_DIR/sbin/apk.static"
if [ ! -x "$APK" ]; then
    curl -sSL -o "$BUILD_DIR/apk-tools-static.apk" \
        "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main/x86_64/$APK_TOOLS_STATIC_PKG"
    mkdir -p "$APK_DIR"
    tar -xzf "$BUILD_DIR/apk-tools-static.apk" -C "$APK_DIR"
fi

# --- build the Alpine package rootfs (idempotent-ish: wipe and reinstall) --
rm -rf "$ROOTFS_TREE"
mkdir -p "$ROOTFS_TREE"

# `add` exits non-zero because of the expected post-install/trigger
# failures described above; the packages themselves land regardless, which
# is what the chroot smoke test above already confirmed.
"$APK" \
    -X "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main" \
    -X "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/community" \
    -U --allow-untrusted --root "$ROOTFS_TREE" --initdb \
    add alpine-baselayout python3 py3-flask \
    || true

if [ ! -x "$ROOTFS_TREE/usr/bin/python3.12" ]; then
    echo "python3 did not land in $ROOTFS_TREE -- apk install failed for real" >&2
    exit 1
fi

# --- guest-init as /init ----------------------------------------------------
cargo build --release --target "$TARGET" -p guest-init \
    --manifest-path "$REPO_ROOT/Cargo.toml"
cp "$REPO_ROOT/target/$TARGET/release/guest-init" "$ROOTFS_TREE/init"
chmod +x "$ROOTFS_TREE/init"

# --- app code + our own launcher/startup script -----------------------------
mkdir -p "$ROOTFS_TREE/app"
cp -r "$APP_SRC/." "$ROOTFS_TREE/app/"
rm -rf "$ROOTFS_TREE/app/.git"

cp "$HERE/run_server.py" "$ROOTFS_TREE/app/run_server.py"
cp "$HERE/start.sh" "$ROOTFS_TREE/app/start.sh"
chmod +x "$ROOTFS_TREE/app/start.sh"

cat > "$ROOTFS_TREE/etc/cirro-init.json" <<'EOF'
{"exec": "/app/start.sh", "args": []}
EOF

# --- pre-initialize the sqlite DB (database_interaction.py's create_db(),
# normally a manual `python3 database_interaction.py` step) so the image
# boots with the two seeded test users/quizzes already in place. Same
# unprivileged user+mount namespace trick as the smoke test above --
# chroot(2) needs CAP_SYS_CHROOT, which an unprivileged user namespace
# grants for itself without touching the real host.
unshare --user --map-root-user --mount -- \
    chroot "$ROOTFS_TREE" /bin/sh -c 'cd /app && /usr/bin/python3 database_interaction.py'

if [ ! -f "$ROOTFS_TREE/app/carrer_quiz.db" ]; then
    echo "carrer_quiz.db was not created -- database_interaction.py failed" >&2
    exit 1
fi

# --- pseudo-filesystem mountpoints guest-init mounts onto at boot ----------
mkdir -p "$ROOTFS_TREE"/{proc,sys,dev}

truncate -s 256M "$ROOTFS_IMG"
mkfs.ext4 -q -d "$ROOTFS_TREE" -F "$ROOTFS_IMG"

echo "rootfs ready: $ROOTFS_IMG"
