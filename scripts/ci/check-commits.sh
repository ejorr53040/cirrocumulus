#!/usr/bin/env bash
# Commit-message and history gate, run by CI on every push/PR and by the
# commit-msg hook (.githooks/commit-msg) on every local commit.
#
# Rules (matching the style this repo's history already uses: imperative,
# sentence-case subject, wrapped prose body):
#   - subject is 1..72 chars, starts with a capital letter, no trailing period
#   - no WIP / fixup! / squash! / amend! commits (squash them before pushing)
#   - if there is a body, line 2 is blank
#   - body lines are <= 100 chars (lines containing a URL are exempt)
#   - range mode only: no merge commits -- history on main stays linear
#
# Usage:
#   scripts/ci/check-commits.sh <base>..<head>   # every commit in a range
#   scripts/ci/check-commits.sh --file <msgfile> # one message (commit-msg hook)

set -euo pipefail

MAX_SUBJECT=72
MAX_BODY=100

fail=0
report() {
    echo "::error::$1: $2"
    fail=1
}

# check_message <label> <message text>
check_message() {
    local label=$1 msg=$2
    local subject second
    subject=$(printf '%s\n' "$msg" | sed -n 1p)
    second=$(printf '%s\n' "$msg" | sed -n 2p)

    if [ -z "$subject" ]; then
        report "$label" "empty subject line"
        return
    fi
    if [ "${#subject}" -gt "$MAX_SUBJECT" ]; then
        report "$label" "subject is ${#subject} chars (max $MAX_SUBJECT): $subject"
    fi
    if [[ "$subject" =~ ^(WIP|wip|fixup!|squash!|amend!) ]]; then
        report "$label" "work-in-progress commit, squash before pushing: $subject"
    elif [[ ! "$subject" =~ ^[A-Z] ]]; then
        report "$label" "subject must start with a capital letter: $subject"
    fi
    if [[ "$subject" =~ \.$ ]]; then
        report "$label" "subject must not end with a period: $subject"
    fi
    if [[ "$subject" =~ [[:space:]]$ ]]; then
        report "$label" "subject has trailing whitespace"
    fi
    if [ -n "$second" ]; then
        report "$label" "line 2 must be blank (separates subject from body)"
    fi

    local n=0 line
    while IFS= read -r line; do
        n=$((n + 1))
        [ "$n" -le 2 ] && continue
        [[ "$line" =~ https?:// ]] && continue
        if [ "${#line}" -gt "$MAX_BODY" ]; then
            report "$label" "body line $n is ${#line} chars (max $MAX_BODY)"
        fi
    done <<< "$msg"
}

case "${1:-}" in
    --file)
        [ $# -eq 2 ] || { echo "usage: $0 --file <msgfile>" >&2; exit 2; }
        # Drop git's comment lines and anything below a `commit -v` scissors line.
        msg=$(sed -e '/^# -\{8,\} >8 -\{8,\}$/,$d' -e '/^#/d' "$2")
        check_message "commit message" "$msg"
        ;;
    *..*)
        range=$1
        count=0
        while IFS= read -r sha; do
            count=$((count + 1))
            short=$(git rev-parse --short "$sha")
            if [ "$(git rev-list --no-walk --count --merges "$sha")" -ne 0 ]; then
                report "$short" "merge commit; rebase instead so history stays linear"
                continue
            fi
            # Dependabot writes its own (long, release-note-laden) bodies;
            # its PR title is still checked by CI's pr-title step.
            if [[ "$(git log -1 --format=%ae "$sha")" == *"dependabot[bot]@users.noreply.github.com" ]]; then
                continue
            fi
            check_message "$short" "$(git log -1 --format=%B "$sha")"
        done < <(git rev-list --reverse "$range")
        echo "checked $count commit(s) in $range"
        ;;
    *)
        echo "usage: $0 <base>..<head> | --file <msgfile>" >&2
        exit 2
        ;;
esac

if [ "$fail" -ne 0 ]; then
    echo "commits: FAILED" >&2
    exit 1
fi
echo "commits: ok"
