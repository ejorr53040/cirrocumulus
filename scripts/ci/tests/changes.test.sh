#!/usr/bin/env bash
# Tests for scripts/ci/changes.sh: each case feeds a list of changed paths
# and asserts which CI gates it turns on. Run by CI's `lint` job.
#
# Usage: scripts/ci/tests/changes.test.sh

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
changes="$here/../changes.sh"

pass=0
fail=0

# expect <name> <expected output> <changed paths, one per line>
expect() {
    local name=$1 want=$2 files=$3 got
    got=$(printf '%s' "$files" | "$changes" --files 2>&1)
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL: $name"
        echo "  wanted: $(printf '%s' "$want" | tr '\n' ' ')"
        echo "  got:    $(printf '%s' "$got" | tr '\n' ' ')"
    fi
}

nothing=$'rust=false\nsupply_chain=false\ncrates='
everything=$'rust=true\nsupply_chain=true\ncrates=all'

expect "docs only" "$nothing" $'CONTEXT.md\ndocs/adr/0001-per-vm-network-namespace.md\nREADME.md'
expect "no files" "$nothing" ""
expect "step0 shell scripts" "$nothing" $'scripts/step0/build_rootfs_nat.sh\nscripts/step0/README.md'
expect "other CI scripts" "$nothing" $'scripts/ci/check-commits.sh\n.githooks/pre-push'

expect "one crate" $'rust=true\nsupply_chain=true\ncrates=cirro-node' \
    'crates/cirro-node/src/network.rs'
expect "two crates, sorted and deduplicated" $'rust=true\nsupply_chain=true\ncrates=cirro-node guest-init' \
    $'crates/guest-init/src/main.rs\ncrates/cirro-node/src/network.rs\ncrates/cirro-node/tests/network.rs'
expect "crate plus docs" $'rust=true\nsupply_chain=true\ncrates=cirro' \
    $'crates/cirro/src/main.rs\nCONTEXT.md'

expect "workspace manifest" "$everything" 'Cargo.toml'
expect "lockfile" "$everything" 'Cargo.lock'
expect "toolchain pin" "$everything" 'rust-toolchain.toml'
expect "cargo config" "$everything" '.cargo/config.toml'
expect "CI workflow" "$everything" '.github/workflows/ci.yml'
expect "test runner" "$everything" 'scripts/ci/test.sh'
expect "this script" "$everything" 'scripts/ci/changes.sh'
expect "workspace file wins over a crate" "$everything" $'crates/cirro-node/src/lib.rs\nCargo.lock'

expect "deny config only" $'rust=false\nsupply_chain=true\ncrates=' 'deny.toml'

got=$("$changes" --all 2>&1)
if [ "$got" = "$everything" ]; then pass=$((pass + 1)); else
    fail=$((fail + 1))
    echo "FAIL: --all: got $(printf '%s' "$got" | tr '\n' ' ')"
fi

echo "changes: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
