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

# M2 slice 3: once the configured app exits, guest-init must shut the VM
# down itself, not park forever waiting for the test harness to kill -9 it.
test_guest_init_boot_shuts_down_when_app_exits() {
    assert_output_contains "FIRECRACKER_EXITED_CLEANLY: PASS" "$STEP0_ROOT/run_guest_init.sh"
}

# "Firecracker's process exited" alone isn't proof of a *clean* shutdown --
# slice 1 found a kernel panic + reboot=k also exits Firecracker with
# exit_code=0. This is what actually distinguishes reboot(RB_POWER_OFF)
# from another panic-triggered auto-reboot.
test_guest_init_boot_shuts_down_without_kernel_panic() {
    assert_output_contains "NO_KERNEL_PANIC: PASS" "$STEP0_ROOT/run_guest_init.sh"
}

# M2 slice 4: child_app (fixtures/child_app.rs) forks a grandchild
# (fixtures/grandchild.rs) and exits without waiting on it, so the
# still-running grandchild is reparented to guest-init (PID 1) -- the
# orphan path a `waitpid` scoped to only the one tracked app pid never
# reaps. If guest-init shuts the VM down the moment the tracked app exits
# (slices 2-3's behavior), the reboot races the grandchild's own exit and
# GRANDCHILD_RAN is typically never seen; guest-init must keep reaping
# until no children remain before shutting down.
test_guest_init_boot_reaps_orphaned_grandchild() {
    assert_output_contains "GRANDCHILD_RAN" "$STEP0_ROOT/run_guest_init.sh"
}
