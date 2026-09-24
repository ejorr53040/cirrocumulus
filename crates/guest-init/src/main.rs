//! `guest-init`: PID 1 inside the Firecracker guest. See RESEARCH.md M2.
//!
//! Slice 1: mount the pseudo-filesystems and print a boot marker. Slice 2:
//! read the configured app from a fixed path, fork, and exec it in the
//! child, with the parent (still PID 1) waiting on it. Slice 3 (current):
//! power off cleanly once that app exits. Reaping other (orphaned) zombies
//! and signal forwarding are still later slices -- this one only tracks
//! the one app child.

mod config;

use config::Config;
use nix::mount::{mount, MsFlags};
use nix::sys::reboot::{reboot, RebootMode};
use nix::sys::wait::waitpid;
use nix::unistd::{execv, fork, ForkResult};
use std::ffi::CString;
use std::io::Write;
use std::path::Path;

/// Fixed path guest-init reads its config from. Stands in for the vsock
/// config channel (RESEARCH.md M2) until that slice lands; same JSON shape
/// either way, so `config::parse_config` doesn't change when the source does.
const CONFIG_PATH: &str = "/etc/cirro-init.json";

/// Printed to the console (ttyS0) once the pseudo-filesystems are mounted,
/// so the boot harness can confirm guest-init reached this point as PID 1.
const BOOT_MARKER: &str = "GUEST_INIT_MOUNTS_OK";

/// Mounts the pseudo-filesystems a guest needs before anything else can
/// run: `/proc` and `/sys` are required (process/kernel introspection);
/// `/dev` (devtmpfs) is best-effort, matching Step 0's own script, since a
/// kernel built without devtmpfs support would otherwise make PID 1 fail
/// outright over a filesystem no one has asked to use yet.
fn mount_pseudo_filesystems() {
    mount(
        Some("proc"),
        Path::new("/proc"),
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .expect("mount /proc");

    mount(
        Some("sysfs"),
        Path::new("/sys"),
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .expect("mount /sys");

    let _ = mount(
        Some("devtmpfs"),
        Path::new("/dev"),
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    );
}

/// Forks and execs the configured app in the child, per RESEARCH.md M2.
/// This is `fork`+`execv`, not a bare `execve` of PID 1 itself: guest-init
/// has to stay running as a distinct process to later reap zombies, forward
/// signals and power off on the app's exit, none of which is possible if
/// the app's image had replaced guest-init's own.
///
/// Blocks until the child exits (the parent's own reap-loop, signal
/// forwarding and shutdown are later slices).
fn exec_configured_app(config: &Config) {
    let exec_path = CString::new(config.exec.as_str()).expect("exec path has no interior NUL");
    let mut argv: Vec<CString> = vec![exec_path.clone()];
    argv.extend(
        config
            .args
            .iter()
            .map(|a| CString::new(a.as_str()).expect("arg has no interior NUL")),
    );

    // SAFETY: single-threaded at this point in boot (PID 1, right after
    // mount), so the child sees a consistent, not-mid-mutation process
    // image between fork and execv.
    match unsafe { fork() }.expect("fork configured app") {
        ForkResult::Child => {
            // execv's Ok is Infallible: it only ever returns on error.
            execv(&exec_path, &argv).expect("execv configured app");
        }
        ForkResult::Parent { child } => {
            waitpid(child, None).expect("waitpid configured app");
        }
    }
}

/// Shuts the VM down. `reboot(2)` doesn't return on success -- the kernel
/// tears the machine down mid-syscall -- which is exactly what PID 1
/// needs: slice 1 already found that *returning* from this point (letting
/// `main` end, or looping and getting killed) panics the guest kernel
/// ("Attempted to kill init!") instead of shutting down cleanly.
///
/// `RB_POWER_OFF` (what RESEARCH.md's M2 method names) turned out not to
/// work here: Firecracker's minimal device model has no ACPI power button,
/// so the kernel finds no `pm_power_off` handler and just halts
/// ("Power off not available: System halted instead") -- the CPU stops but
/// the Firecracker process stays alive, doing nothing, forever. slice 1's
/// panic trace already showed the mechanism Firecracker actually reacts
/// to: a reboot (the same x86 reset path a kernel panic + `reboot=k`
/// takes), which Firecracker traps and exits the VMM on. `RB_AUTOBOOT`
/// takes that same path deliberately instead of via a panic.
fn shutdown() -> ! {
    match reboot(RebootMode::RB_AUTOBOOT) {
        Ok(never) => match never {},
        Err(e) => panic!("reboot RB_AUTOBOOT failed: {e}"),
    }
}

fn main() {
    mount_pseudo_filesystems();

    println!("{BOOT_MARKER}");
    // A println isn't enough on its own: the guest kernel's serial driver
    // can still be draining this write into the UART when the next line
    // runs, and PID 1 returning from main is fatal (the kernel panics with
    // "Attempted to kill init!"), tearing down the console mid-write and
    // truncating or dropping the marker entirely. Flush explicitly, then
    // never return: reap/exec/shutdown land in later slices, but even this
    // mount-only slice must hold PID 1 open, because a guest's init isn't
    // allowed to exit at all.
    std::io::stdout().flush().expect("flush boot marker");

    let config_json = std::fs::read_to_string(CONFIG_PATH)
        .unwrap_or_else(|e| panic!("read config at {CONFIG_PATH}: {e}"));
    let config = config::parse_config(&config_json).expect("parse config");
    exec_configured_app(&config);

    shutdown();
}
