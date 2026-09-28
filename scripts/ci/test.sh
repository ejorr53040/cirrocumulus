#!/usr/bin/env bash
# The hermetic test suite CI runs. Everything is *compiled* (so the
# real-Firecracker tests in crates/cirro-node/tests can't bit-rot), but only
# tests that need no VM assets are *run*.
#
# Why not just `cargo test --workspace`: GitHub's Linux runners expose
# /dev/kvm and passwordless sudo, so the cirro-node VM tests would get past
# their skip checks and then panic on the missing kernel/rootfs under
# scripts/step0/.build/. Run those locally per scripts/step0/README.md.
#
# Usage: scripts/ci/test.sh

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

cargo test --workspace --locked --all-targets --no-run
cargo test --workspace --locked --lib --bins --test cli
cargo test --workspace --locked --doc
