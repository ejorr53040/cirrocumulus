// M2 test fixture: the app guest-init forks+execs. Prints its own marker
// then exits promptly (slice 3 needs a real exit to react to -- guest-init
// shuts the VM down when this process ends). Flushed explicitly: this
// isn't PID 1, so its own exit can't panic the kernel, but an unflushed
// write racing process teardown can still be dropped, same lesson slice 1
// learned the hard way.
//
// Slice 4: also spawns grandchild.rs and deliberately never waits on it
// (dropping a std::process::Child without .wait() does not reap it) --
// this process exits with the grandchild still running, so it gets
// reparented to guest-init, the orphan path slice 4's reap loop covers.
use std::io::Write;
use std::process::Command;

fn main() {
    let _grandchild = Command::new("/app/grandchild")
        .spawn()
        .expect("spawn grandchild");

    println!("CHILD_APP_RAN");
    std::io::stdout().flush().expect("flush marker");
}
