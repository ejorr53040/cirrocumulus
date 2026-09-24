#!/usr/bin/env bash
# M2 slice 5: a host-triggered shutdown (Firecracker's SendCtrlAltDel
# action) must reach the app as a real SIGTERM, not just hard-reset the
# whole guest out from under it. long_running_app.rs (fixtures/) never
# exits on its own, so the only way run_guest_init_cad.sh ever sees
# FIRECRACKER_EXITED_CLEANLY is via this path.

test_guest_init_shutdown_app_starts() {
    assert_output_contains "LONG_RUNNING_APP_STARTED" "$STEP0_ROOT/run_guest_init_cad.sh"
}

# The one assertion that actually distinguishes "guest-init forwarded a
# real SIGTERM to the app" from "the kernel hard-reset the machine before
# any userspace code ran": guest-init prints this the moment it forwards
# the signal, which is only reachable if Ctrl-Alt-Del reached PID 1 as a
# SIGINT (soft CAD) rather than triggering an immediate kernel restart
# (hard CAD, the default guest-init has to explicitly opt out of).
test_guest_init_shutdown_forwards_sigterm_to_app() {
    assert_output_contains "GUEST_INIT_FORWARDING_SIGTERM" "$STEP0_ROOT/run_guest_init_cad.sh"
}

test_guest_init_shutdown_exits_cleanly() {
    assert_output_contains "FIRECRACKER_EXITED_CLEANLY: PASS" "$STEP0_ROOT/run_guest_init_cad.sh"
}

test_guest_init_shutdown_without_kernel_panic() {
    assert_output_contains "NO_KERNEL_PANIC: PASS" "$STEP0_ROOT/run_guest_init_cad.sh"
}
