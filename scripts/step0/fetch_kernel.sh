#!/usr/bin/env bash
# Downloads the latest Firecracker CI-built vmlinux kernel image for this
# arch, per the upstream quickstart guide:
# https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
# Idempotent: skips the download if a vmlinux* file is already present.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="$HERE/.build"
mkdir -p "$BUILD_DIR"

if ls "$BUILD_DIR"/vmlinux-* >/dev/null 2>&1; then
    echo "kernel already present: $(ls "$BUILD_DIR"/vmlinux-* | tail -1)"
    exit 0
fi

ARCH="$(uname -m)"
S3="https://s3.amazonaws.com/spec.ccfc.min"

CI_ARTIFACTS_PREFIX=$(curl -fsSL "$S3?list-type=2&prefix=firecracker-ci/&delimiter=/" \
    | grep -oP "(?<=<Prefix>)firecracker-ci/[0-9]{8}-[^/]+/(?=</Prefix>)" \
    | sort | tail -1)

latest_kernel_key=$(curl -fsSL "$S3?list-type=2&prefix=${CI_ARTIFACTS_PREFIX}${ARCH}/vmlinux-" \
    | grep -oP "(?<=<Key>)(${CI_ARTIFACTS_PREFIX}${ARCH}/vmlinux-[0-9]+\.[0-9]+\.[0-9]{1,3})(?=</Key>)" \
    | sort -V | tail -1)

if [ -z "$latest_kernel_key" ]; then
    echo "could not find a kernel artifact under prefix ${CI_ARTIFACTS_PREFIX}${ARCH}/" >&2
    exit 1
fi

echo "downloading $S3/$latest_kernel_key"
wget -q -O "$BUILD_DIR/$(basename "$latest_kernel_key")" "$S3/$latest_kernel_key"
echo "kernel ready: $BUILD_DIR/$(basename "$latest_kernel_key")"
