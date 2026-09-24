// M2 test fixture: the app guest-init forks+execs. Prints its own marker
// then exits promptly (slice 3 needs a real exit to react to -- guest-init
// shuts the VM down when this process ends). Flushed explicitly: this
// isn't PID 1, so its own exit can't panic the kernel, but an unflushed
// write racing process teardown can still be dropped, same lesson slice 1
// learned the hard way.
use std::io::Write;

fn main() {
    println!("CHILD_APP_RAN");
    std::io::stdout().flush().expect("flush marker");
}
