#!/usr/bin/env bash
# Runs every tests/test_*.sh file in this directory. A test file defines one
# or more test_* functions; each is called with no arguments and must return
# 0 (pass) or non-zero (fail, after printing why).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STEP0_ROOT="$(cd "$HERE/.." && pwd)"
export STEP0_ROOT

# shellcheck source=lib/assert.sh
source "$HERE/lib/assert.sh"

pass=0
fail=0
failed_names=()

for f in "$HERE"/test_*.sh; do
    [ -e "$f" ] || continue
    # shellcheck source=/dev/null
    source "$f"
done

for fn in $(declare -F | awk '{print $3}' | grep '^test_' | sort); do
    echo "=== $fn ==="
    if "$fn"; then
        echo "PASS: $fn"
        pass=$((pass + 1))
    else
        echo "FAIL: $fn"
        fail=$((fail + 1))
        failed_names+=("$fn")
    fi
    echo
done

echo "----------------------------------------"
echo "$pass passed, $fail failed"
if [ "$fail" -ne 0 ]; then
    printf 'failed: %s\n' "${failed_names[@]}"
    exit 1
fi
