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
# Given crate directories (as scripts/ci/changes.sh reports them), only
# those crates and every workspace crate that depends on them are compiled
# and tested. With no arguments, or `all`, the whole workspace is.
#
# Usage: scripts/ci/test.sh [all | <crate-dir>...]   # e.g. cirro-node guest-init

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [ $# -eq 0 ] || [ "$*" = all ]; then
    cargo test --workspace --locked --all-targets --no-run
    cargo test --workspace --locked --lib --bins --test cli
    cargo test --workspace --locked --doc
    exit 0
fi

# Turn crate dirs into a test plan, one line per affected package:
#   <package> <cargo test flags for its hermetic targets...>
# `-p` makes cargo reject flags a package has no target for (`--lib` on a
# bin-only crate, `--test cli` outside the CLI crate), so the flags are
# chosen per package from `cargo metadata`.
plan_py='
import json, sys

HERMETIC_TESTS = {"cli"}
# Dependencies cargo metadata cannot see: cirro'"'"'s build.rs compiles
# guest-init and embeds the binary, so a guest-init change must retest cirro.
EMBEDS = {"guest-init": ["cirrocumulus"]}

meta = json.load(sys.stdin)
wanted_dirs = set(sys.argv[1:])
by_name = {p["name"]: p for p in meta["packages"]}
crate_dir = {p["name"]: p["manifest_path"].split("/crates/")[1].split("/")[0]
             for p in meta["packages"]}

dependents = {name: set(EMBEDS.get(name, [])) for name in by_name}
for p in meta["packages"]:
    for d in p["dependencies"]:
        if d.get("path") and d["name"] in dependents:
            dependents[d["name"]].add(p["name"])

todo = [n for n, d in crate_dir.items() if d in wanted_dirs]
selected = set()
while todo:
    name = todo.pop()
    if name not in selected:
        selected.add(name)
        todo.extend(dependents[name])

for name in sorted(selected):
    kinds = {k for t in by_name[name]["targets"] for k in t["kind"]}
    tests = sorted(t["name"] for t in by_name[name]["targets"]
                   if "test" in t["kind"] and t["name"] in HERMETIC_TESTS)
    flags = ([ "--lib", "--doc"] if "lib" in kinds else []) \
        + (["--bins"] if "bin" in kinds else []) \
        + [f for t in tests for f in ("--test", t)]
    print(name, *flags)
'

mapfile -t plan < <(cargo metadata --format-version 1 --no-deps --locked |
    python3 -c "$plan_py" "$@")

if [ "${#plan[@]}" -eq 0 ]; then
    echo "test: no workspace crates in: $*"
    exit 0
fi

packages=()
for line in "${plan[@]}"; do packages+=(-p "${line%% *}"); done
echo "test: ${plan[*]%% *}"
cargo test --locked --all-targets --no-run "${packages[@]}"

for line in "${plan[@]}"; do
    read -r name flags <<< "$line"
    run_flags=() doc=0
    for f in $flags; do
        if [ "$f" = --doc ]; then doc=1; else run_flags+=("$f"); fi
    done
    if [ "${#run_flags[@]}" -gt 0 ]; then
        cargo test --locked -p "$name" "${run_flags[@]}"
    fi
    if [ "$doc" -eq 1 ]; then
        cargo test --locked -p "$name" --doc
    fi
done
