#!/usr/bin/env bash
# Minimal assertion helpers for the step0 shell test suite. No external deps
# (bats-core etc. aren't installed and step0 is meant to be runnable with
# nothing beyond the base OS + the firecracker/jailer binaries).

assert_exit_code() {
    local expected="$1"; shift
    local out
    out="$("$@" 2>&1)"
    local rc=$?
    if [ "$rc" -ne "$expected" ]; then
        echo "  expected exit code $expected, got $rc for: $*"
        echo "  output:"
        echo "$out" | sed 's/^/    /'
        return 1
    fi
    return 0
}

assert_output_contains() {
    local needle="$1"; shift
    local out
    out="$("$@" 2>&1)"
    local rc=$?
    case "$out" in
        *"$needle"*) return 0 ;;
        *)
            echo "  expected output to contain: $needle"
            echo "  exit code: $rc, output:"
            echo "$out" | sed 's/^/    /'
            return 1
            ;;
    esac
}
