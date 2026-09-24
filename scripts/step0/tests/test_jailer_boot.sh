#!/usr/bin/env bash
# Seam 3: the actual target of this task -- jailer wraps firecracker in a
# chroot, drops privileges to an unprivileged uid/gid, and the same
# kernel+rootfs still boots to userspace. Public interface is run_jailer.sh:
# callers only observe its exit code and console output, never the chroot
# internals directly.

test_jailer_boot_reaches_userspace() {
    assert_exit_code 0 "$STEP0_ROOT/run_jailer.sh"
}

test_jailer_boot_console_shows_marker() {
    assert_output_contains "STEP0_BOOT_OK" "$STEP0_ROOT/run_jailer.sh"
}

test_jailer_boot_runs_as_unprivileged_uid() {
    # The whole point of the jailer vs. run_plain.sh: firecracker itself
    # must not run as root inside the jail, even though jailer needs root
    # to set the jail up.
    assert_output_contains "firecracker not running as root: PASS" "$STEP0_ROOT/run_jailer.sh"
}
