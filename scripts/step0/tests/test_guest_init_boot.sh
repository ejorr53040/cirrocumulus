#!/usr/bin/env bash
# M2 seam: guest-init (crates/guest-init) as the real PID 1, replacing
# Step 0's busybox /init script. Public interface is run_guest_init.sh --
# callers only observe its exit code and serial console output, same
# pattern as test_plain_boot.sh's busybox seam.

test_guest_init_boot_reaches_userspace() {
    assert_exit_code 0 "$STEP0_ROOT/run_guest_init.sh"
}

test_guest_init_boot_console_shows_marker() {
    assert_output_contains "GUEST_INIT_MOUNTS_OK" "$STEP0_ROOT/run_guest_init.sh"
}
