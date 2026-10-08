#!/usr/bin/env bash
# Downloads a pinned, SHA-256-verified Firecracker CI-built vmlinux kernel
# image for x86_64, per the upstream quickstart guide:
# https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
# Idempotent: skips the download if a vmlinux* file is already present.
#
# 2026-09-28 security audit, #3: this used to fetch whatever CI happened to
# have under the "latest" date-stamped prefix, over HTTPS but with no
# integrity check afterward -- the one artifact in the whole boot chain
# that's KVM/jailer/cgroups' own peer in the isolation stack (RESEARCH.md's
# threat model), fetched with zero verification. Pinned to the same
# kernel+hash `cirro node install` (#11, `crates/cirro-node/src/release.rs`)
# already verifies, so Step 0's dev harness and a real Node install trust
# the exact same bytes. Re-pin (and update `release.rs` to match) by
# removing the pin below, running this script's old always-latest logic by
# hand once, then `sha256sum` the result.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="$HERE/.build"
mkdir -p "$BUILD_DIR"

# Keep in sync with crates/cirro-node/src/release.rs's KERNEL_URL/KERNEL_SHA256.
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260929-a738f18a8db0-0/x86_64/vmlinux-6.18.48"
KERNEL_SHA256="b0ff002711a6be32f2f5cbc21fbb7b2987b7807e0d37540034d49db22ce3d06b"
KERNEL_FILE="$BUILD_DIR/$(basename "$KERNEL_URL")"

verify() {
    echo "$KERNEL_SHA256  $KERNEL_FILE" | sha256sum -c - >/dev/null 2>&1
}

if [ -f "$KERNEL_FILE" ]; then
    if verify; then
        echo "kernel already present and verified: $KERNEL_FILE"
        exit 0
    fi
    echo "$KERNEL_FILE exists but doesn't match the pinned SHA-256; re-fetching" >&2
    rm -f "$KERNEL_FILE"
fi

echo "downloading $KERNEL_URL"
curl -fsSL -o "$KERNEL_FILE" "$KERNEL_URL"
if ! verify; then
    echo "downloaded kernel doesn't match the pinned SHA-256 ($KERNEL_SHA256); refusing to use it" >&2
    rm -f "$KERNEL_FILE"
    exit 1
fi
echo "kernel ready and verified: $KERNEL_FILE"
