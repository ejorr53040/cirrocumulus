#!/usr/bin/env bash
# Decides which CI gates a change actually needs, so a docs-only or
# shell-only push doesn't spend 30+ minutes compiling Rust. Prints three
# `key=value` lines, ready to append to $GITHUB_OUTPUT:
#
#   rust=true|false          fmt, clippy, rustdoc, tests, every-commit-builds
#   supply_chain=true|false  cargo-deny, cargo-machete
#   crates=all|<dirs>        which crates/<dir>s to test ("" when rust=false)
#
# A change to anything workspace-wide (manifests, lockfile, toolchain, cargo
# config, the CI workflow, or the test/selection scripts themselves) turns
# everything on. Otherwise only the crates whose files changed are listed;
# scripts/ci/test.sh expands that to the crates that depend on them.
#
# The git and lint jobs don't consult this: commit checks and spelling
# apply to every change, and they take seconds.
#
# Usage:
#   scripts/ci/changes.sh <base>..<head>   # files changed in a commit range
#   scripts/ci/changes.sh --files          # changed paths on stdin, one per line
#   scripts/ci/changes.sh --all            # scheduled/manual runs: everything

set -euo pipefail

emit() {
    printf 'rust=%s\nsupply_chain=%s\ncrates=%s\n' "$1" "$2" "$3"
}

case "${1:-}" in
    --all)
        emit true true all
        exit 0
        ;;
    --files)
        mapfile -t files
        ;;
    *..*)
        cd "$(git rev-parse --show-toplevel)"
        mapfile -t files < <(git diff --name-only "$1")
        ;;
    *)
        echo "usage: $0 <base>..<head> | --files | --all" >&2
        exit 2
        ;;
esac

supply_chain=false
declare -A crates=()
for f in "${files[@]}"; do
    case "$f" in
        Cargo.toml | Cargo.lock | rust-toolchain.toml | .cargo/* | \
            .github/workflows/ci.yml | scripts/ci/test.sh | scripts/ci/changes.sh)
            emit true true all
            exit 0
            ;;
        deny.toml)
            supply_chain=true
            ;;
        crates/*/*)
            dir=${f#crates/}
            crates[${dir%%/*}]=1
            ;;
    esac
done

if [ "${#crates[@]}" -eq 0 ]; then
    emit false "$supply_chain" ""
else
    emit true true "$(printf '%s\n' "${!crates[@]}" | sort | paste -sd ' ')"
fi
