#!/usr/bin/env bash
# Fetches the target app (https://github.com/VERSO-UVM/UVM-Career-Quiz) at a
# pinned commit, for build_rootfs.sh to package. Pinned rather than tracking
# main so a rootfs build today and one next month package the same code.
# Idempotent: skips the clone if already present at that commit.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="$HERE/.build"
APP_SRC="$BUILD_DIR/app-src"
REPO_URL="https://github.com/VERSO-UVM/UVM-Career-Quiz.git"
# Full SHA: a 7-char prefix can name a different commit as the history grows.
PINNED_COMMIT="1e9d686e69664c4812e0417cf38e9479811fda5b"

mkdir -p "$BUILD_DIR"

if [ -d "$APP_SRC/.git" ] && [ "$(git -C "$APP_SRC" rev-parse HEAD 2>/dev/null)" = "$PINNED_COMMIT" ]; then
    echo "app source already present at $PINNED_COMMIT: $APP_SRC"
    exit 0
fi

rm -rf "$APP_SRC"
git clone -q "$REPO_URL" "$APP_SRC"
git -C "$APP_SRC" checkout -q "$PINNED_COMMIT"
echo "app source ready at $PINNED_COMMIT: $APP_SRC"
