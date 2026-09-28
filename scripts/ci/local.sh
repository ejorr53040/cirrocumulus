#!/usr/bin/env bash
# Runs the same gates as .github/workflows/ci.yml, locally, stopping at the
# first failure. The pre-push hook runs `--quick` for you once
# scripts/ci/install-hooks.sh has been run.
#
# Tools CI installs that you may not have locally (shellcheck, typos,
# actionlint, cargo-deny, cargo-machete) are skipped with a warning rather
# than failing -- CI still runs them.
#
# Usage: scripts/ci/local.sh [--quick]   # --quick skips rustdoc and tests

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

quick=0
[ "${1:-}" = "--quick" ] && quick=1

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
have() {
    command -v "$1" > /dev/null 2>&1 && return 0
    printf '\033[33mskip: %s not installed (CI still runs it)\033[0m\n' "$1"
    return 1
}

step "hygiene"
scripts/ci/check-hygiene.sh
step "CI script tests"
scripts/ci/tests/check-commits.test.sh
step "rustfmt"
cargo fmt --all --check
step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
if [ "$quick" -eq 0 ]; then
    step "rustdoc"
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
    step "tests"
    scripts/ci/test.sh
fi
step "shellcheck"
if have shellcheck; then scripts/ci/check-shell.sh; fi
step "typos"
if have typos; then typos; fi
step "actionlint"
if have actionlint; then actionlint; fi
step "cargo-deny"
if have cargo-deny; then cargo deny --locked check; fi
step "cargo-machete"
if have cargo-machete; then cargo machete; fi

printf '\n\033[32mall local CI gates passed\033[0m\n'
