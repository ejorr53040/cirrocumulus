// M2 slice 4 fixture: spawned by child_app.rs and deliberately not waited
// on, so it is still running when child_app exits -- Linux reparents it to
// guest-init (PID 1) at that point, which is the orphan-reparent path
// slice 4's waitpid(-1) reap loop needs to exercise. Sleeps briefly first
// so it reliably outlives child_app's own near-instant exit, then prints
// its own marker so the boot test can see whether guest-init stayed up
// long enough to let it finish before shutting the VM down.
use std::io::Write;
use std::{thread, time::Duration};

fn main() {
    thread::sleep(Duration::from_millis(500));
    println!("GRANDCHILD_RAN");
    std::io::stdout().flush().expect("flush marker");
}
