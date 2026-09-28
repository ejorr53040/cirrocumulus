#!/usr/bin/env bash
# Points this clone's git hooks at .githooks/ (commit-msg + pre-push), so
# the commit-message and lint gates CI enforces run before anything leaves
# your machine. Undo with: git config --unset core.hooksPath
#
# Usage: scripts/ci/install-hooks.sh

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "git hooks installed (core.hooksPath=.githooks)"
