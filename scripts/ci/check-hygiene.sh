#!/usr/bin/env bash
# Repo hygiene gate, run by CI (.github/workflows/ci.yml) and the pre-push
# hook (.githooks/pre-push). Checks every *tracked* file, so it sees exactly
# what a clone would get -- untracked scratch files and gitignored build
# output (firecracker/jailer binaries, rootfs images) are out of scope.
#
# Fails on:
#   - files over MAX_KB (keeps the repo cloneable; VM images belong in .build/)
#   - committed ELF binaries (firecracker/jailer are fetched, never committed)
#   - tracked files that .gitignore says should be ignored
#   - unresolved merge-conflict markers
#   - CRLF line endings, trailing whitespace, missing final newline
#   - shell scripts without a shebang or without the executable bit
#
# Usage: scripts/ci/check-hygiene.sh

set -euo pipefail

MAX_KB=512

cd "$(git rev-parse --show-toplevel)"

fail=0
report() {
    echo "::error file=$1::$2"
    fail=1
}

# Binary-ish files that are allowed to skip the text checks.
is_text() {
    # `grep -I` treats a file with NUL bytes as binary and prints nothing.
    [ ! -s "$1" ] || grep -Iq . "$1"
}

while IFS= read -r -d '' f; do
    [ -f "$f" ] || continue # deleted in the working tree but still in the index

    size_kb=$(( $(stat -c %s "$f") / 1024 ))
    if [ "$size_kb" -gt "$MAX_KB" ]; then
        report "$f" "file is ${size_kb} KiB, over the ${MAX_KB} KiB limit"
    fi

    if [ "$(head -c 4 "$f" | od -An -tx1 | tr -d ' \n')" = "7f454c46" ]; then
        report "$f" "ELF binary committed; build or fetch it instead"
        continue
    fi

    is_text "$f" || continue

    if grep -qn $'\r' "$f"; then
        report "$f" "CRLF line endings"
    fi
    if line=$(grep -nE '[[:blank:]]+$' "$f" | head -1 | cut -d: -f1) && [ -n "$line" ]; then
        # Markdown uses two trailing spaces as a hard line break.
        case "$f" in
            *.md) ;;
            *) report "$f" "trailing whitespace (first at line $line)" ;;
        esac
    fi
    if [ -s "$f" ] && [ "$(tail -c 1 "$f" | od -An -tx1 | tr -d ' ')" != "0a" ]; then
        report "$f" "missing final newline"
    fi
    if line=$(grep -nE '^(<{7}|>{7}|={7})( |$)' "$f" | head -1 | cut -d: -f1) && [ -n "$line" ]; then
        report "$f" "merge-conflict marker at line $line"
    fi

    case "$f" in
        *.sh)
            if [ "$(head -c 2 "$f")" != "#!" ]; then
                report "$f" "shell script has no shebang"
            fi
            # Sourced, not executed: scripts/step0/tests/lib/*.sh helpers, and
            # the test_*.sh files scripts/step0/tests/run.sh sources.
            case "$f" in
                */lib/* | */tests/test_*.sh) ;;
                *) [ -x "$f" ] || report "$f" "shell script is not executable (git update-index --chmod=+x)" ;;
            esac
            ;;
    esac
done < <(git ls-files -z)

while IFS= read -r f; do
    report "$f" "tracked but matched by .gitignore (git rm --cached it, or fix .gitignore)"
done < <(git ls-files -ci --exclude-standard)

if [ "$fail" -ne 0 ]; then
    echo "hygiene: FAILED" >&2
    exit 1
fi
echo "hygiene: ok"
