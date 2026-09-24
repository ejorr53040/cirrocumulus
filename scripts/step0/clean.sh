#!/usr/bin/env bash
# Reclaims the disk space run_jailer.sh leaves behind. jailer only chowns
# the leaf jail directory to the unprivileged uid, not the "firecracker/"
# and "<id>/" directories above it, so each test run gets its own id
# instead of deleting the last one -- see README.md. This needs sudo.
set -euo pipefail

JAIL_DIR="$HOME/.cirrocumulus-step0-jail"

if [ ! -d "$JAIL_DIR" ]; then
    echo "nothing to clean: $JAIL_DIR does not exist"
    exit 0
fi

echo "removing $JAIL_DIR ($(du -sh "$JAIL_DIR" 2>/dev/null | cut -f1))"
sudo rm -rf "$JAIL_DIR"
