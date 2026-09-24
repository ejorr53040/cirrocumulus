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

# M2 slice 2: guest-init reads /etc/cirro-init.json, forks, and execs the
# configured app. child_app.rs (fixtures/) prints its own marker, so seeing
# it on the console proves the fork+exec actually ran the app as a child of
# PID 1, not just that guest-init itself is alive.
test_guest_init_boot_execs_configured_app() {
    assert_output_contains "CHILD_APP_RAN" "$STEP0_ROOT/run_guest_init.sh"
}
