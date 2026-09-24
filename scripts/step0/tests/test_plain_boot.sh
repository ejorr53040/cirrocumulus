#!/usr/bin/env bash
# Seam 2: plain firecracker (no jailer) boots the kernel+rootfs built by
# build_rootfs.sh/fetch_kernel.sh all the way to guest userspace. Public
# interface is "run the microVM via the Firecracker API socket and observe
# the console" -- exactly what a caller of run_plain.sh gets.

test_plain_boot_reaches_userspace() {
    assert_exit_code 0 "$STEP0_ROOT/run_plain.sh"
}

test_plain_boot_console_shows_marker() {
    assert_output_contains "STEP0_BOOT_OK" "$STEP0_ROOT/run_plain.sh"
}
