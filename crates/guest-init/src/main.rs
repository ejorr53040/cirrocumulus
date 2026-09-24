//! `guest-init`: PID 1 inside the Firecracker guest. See RESEARCH.md M2.
//!
//! Slice 1 (current): mount the pseudo-filesystems the guest needs and
//! print a boot marker to the serial console, mirroring what Step 0's
//! busybox `/init` script did by hand. Exec/reap/shutdown/vsock follow in
//! later slices.

use nix::mount::{mount, MsFlags};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

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
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
