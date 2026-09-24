#!/usr/bin/env bash
# Seam 1: the prereq check. Public interface is the exit code and report of
# scripts/step0/prereqs.sh -- callers (a human, CI) only observe that.

test_prereqs_passes_on_a_ready_machine() {
    assert_exit_code 0 "$STEP0_ROOT/prereqs.sh"
}

test_prereqs_reports_each_check() {
    assert_output_contains "KVM" "$STEP0_ROOT/prereqs.sh" &&
    assert_output_contains "cgroup" "$STEP0_ROOT/prereqs.sh" &&
    assert_output_contains "firecracker" "$STEP0_ROOT/prereqs.sh" &&
    assert_output_contains "jailer" "$STEP0_ROOT/prereqs.sh"
}
