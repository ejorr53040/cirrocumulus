#!/usr/bin/env bash
# Tests for scripts/ci/check-commits.sh: each case writes a commit message
# and asserts the checker accepts or rejects it. Run by CI's `lint` job.
#
# Usage: scripts/ci/tests/check-commits.test.sh

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
checker="$here/../check-commits.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

pass=0
fail=0

# expect <accept|reject> <name> <message>
expect() {
    local want=$1 name=$2 msg=$3 got
    printf '%s\n' "$msg" > "$tmp/msg"
    if "$checker" --file "$tmp/msg" > "$tmp/out" 2>&1; then got=accept; else got=reject; fi
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL: $name: wanted $want, got $got"
        sed 's/^/    /' "$tmp/out"
    fi
}

long_body_line=$(printf 'x%.0s' {1..101})
long_url_line="See https://example.com/$(printf 'y%.0s' {1..100})"
subject_72=$(printf 'A%.0s' {1..72})
subject_73=$(printf 'A%.0s' {1..73})

expect accept "subject only" "Add strict CI"
expect accept "subject and body" $'Add strict CI\n\nExplains why.'
expect accept "72-char subject" "$subject_72"
expect accept "long line with a URL" $'Add strict CI\n\n'"$long_url_line"
expect accept "git comments stripped" $'Add strict CI\n\n# '"$long_body_line"
expect accept "scissors section stripped" $'Add strict CI\n\n# ------------------------ >8 ------------------------\n'"$long_body_line"

expect reject "empty message" ""
expect reject "lowercase subject" "some more vm stuff"
expect reject "trailing period" "Fix the thing."
expect reject "73-char subject" "$subject_73"
expect reject "fixup commit" "fixup! Add NAT"
expect reject "squash commit" "squash! Add NAT"
expect reject "WIP commit" "WIP networking"
expect reject "no blank line 2" $'Add strict CI\nbody right away'
expect reject "long body line" $'Add strict CI\n\n'"$long_body_line"
expect reject "trailing space in subject" "Add strict CI "

echo "check-commits: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
