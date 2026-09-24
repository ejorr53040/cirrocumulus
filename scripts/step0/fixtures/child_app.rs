// M2 slice 2 test fixture: the app guest-init forks+execs. Just proves the
// exec happened -- prints its own marker, then parks so it doesn't exit
// before the boot harness can read the console (guest-init's reap/shutdown
// behavior on child exit is a later slice, not this one).
fn main() {
    println!("CHILD_APP_RAN");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
