#!/usr/bin/env bash
# Builds small test Apps as ext4 rootfs images for exercising a Node by
# hand: each is Alpine's busybox plus guest-init as /init, with one
# /entrypoint script. No Docker, no root -- the same apk.static approach as
# scripts/apps/uvm-career-quiz/build_rootfs.sh, whose comments explain it.
#
#   scripts/test-apps/build.sh             # every app
#   scripts/test-apps/build.sh hello-http  # just these
#
# Images land in scripts/test-apps/.build/<app>.ext4; README.md says what
# each one is for and how to run it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
BUILD_DIR="$HERE/.build"
BASE_TREE="$BUILD_DIR/base"
TARGET="x86_64-unknown-linux-musl"
ALPINE_BRANCH="v3.20"
APK_TOOLS_STATIC_PKG="apk-tools-static-2.14.4-r1.apk"

apps=("$@")
if [ ${#apps[@]} -eq 0 ]; then
    for dir in "$HERE"/apps/*/; do apps+=("$(basename "$dir")"); done
fi
for app in "${apps[@]}"; do
    [ -d "$HERE/apps/$app" ] || { echo "no app $app in $HERE/apps" >&2; exit 1; }
done

mkdir -p "$BUILD_DIR"

# --- Alpine's static apk tool -----------------------------------------------
APK_DIR="$BUILD_DIR/apk-tools-static"
APK="$APK_DIR/sbin/apk.static"
if [ ! -x "$APK" ]; then
    curl -sSfL -o "$BUILD_DIR/apk-tools-static.apk" \
        "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main/x86_64/$APK_TOOLS_STATIC_PKG"
    mkdir -p "$APK_DIR"
    tar -xzf "$BUILD_DIR/apk-tools-static.apk" -C "$APK_DIR" 2>/dev/null
fi

# --- guest-init ---------------------------------------------------------------
cargo build --release --target "$TARGET" -p cirro-guest-init --bin guest-init \
    --manifest-path "$REPO_ROOT/Cargo.toml"
GUEST_INIT="$REPO_ROOT/target/$TARGET/release/guest-init"

# --- the shared base tree: busybox and its applet symlinks ------------------
if [ ! -x "$BASE_TREE/bin/busybox" ]; then
    rm -rf "$BASE_TREE"
    mkdir -p "$BASE_TREE"
    # Exits non-zero over the post-install scripts that need real root;
    # the packages land regardless (see build_rootfs.sh).
    "$APK" \
        -X "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main" \
        --keys-dir "$REPO_ROOT/scripts/apps/uvm-career-quiz/alpine-keys" \
        -U --root "$BASE_TREE" --initdb \
        add alpine-baselayout busybox busybox-extras \
        >/dev/null 2>&1 || true
    if [ ! -x "$BASE_TREE/bin/busybox" ]; then
        echo "busybox did not land in $BASE_TREE -- apk install failed for real" >&2
        exit 1
    fi
    # The applet symlink farm busybox's own trigger would have made.
    unshare --user --map-root-user --mount -- \
        chroot "$BASE_TREE" /bin/busybox --install -s
    # httpd lives in busybox-extras, which the farm above doesn't cover.
    ln -s /bin/busybox-extras "$BASE_TREE/usr/sbin/httpd"
    # A rootfs path, unlike a pulled image, gets no resolv.conf from cirro.
    printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\n' > "$BASE_TREE/etc/resolv.conf"
    mkdir -p "$BASE_TREE"/{proc,sys,dev,tmp,run}
    chmod 1777 "$BASE_TREE/tmp"
fi

# --- one rootfs per app -------------------------------------------------------
for app in "${apps[@]}"; do
    tree="$BUILD_DIR/tree-$app"
    img="$BUILD_DIR/$app.ext4"
    rm -rf "$tree"
    cp -a "$BASE_TREE" "$tree"
    cp -r "$HERE/apps/$app/." "$tree/"
    install -m 755 "$GUEST_INIT" "$tree/init"
    chmod 755 "$tree/entrypoint"
    if [ -d "$tree/www/cgi-bin" ]; then chmod 755 "$tree"/www/cgi-bin/*; fi
    rm -f "$img"
    truncate -s 64M "$img"
    mkfs.ext4 -q -d "$tree" -F "$img"
    rm -rf "$tree"
    echo "built $img"
done
