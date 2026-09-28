#!/usr/bin/env bash
# ShellCheck every tracked shell script (and the git hooks) at the strictest
# severity. `-x` follows `source`d files (scripts/step0/tests/lib/assert.sh)
# so sourced helpers are linted in context. Config lives in .shellcheckrc.
#
# Usage: scripts/ci/check-shell.sh

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

mapfile -d '' scripts < <(git ls-files -z '*.sh' .githooks)
shellcheck --version | sed -n 2p
shellcheck -x --severity=style "${scripts[@]}"
echo "shell: ok (${#scripts[@]} scripts)"
