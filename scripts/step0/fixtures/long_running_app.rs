// M2 slice 5 fixture: an app that never exits on its own -- something
// external (a host-triggered shutdown, e.g. Firecracker's SendCtrlAltDel
// action) has to end it, unlike child_app.rs which always exits promptly.
// Prints a start marker so the boot test knows it's actually running, then
// parks. Deliberately has no signal handling of its own: guest-init is
// what's supposed to translate the host's shutdown request into a SIGTERM
// here, and the default SIGTERM disposition (process termination) is
// exactly what a real app not written for and knowing about this would do.
use std::io::Write;
use std::{thread, time::Duration};

fn main() {
    println!("LONG_RUNNING_APP_STARTED");
    std::io::stdout().flush().expect("flush marker");

    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
