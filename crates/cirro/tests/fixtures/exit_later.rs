// M3 fixture: a command that exits on its own, cleanly, a few seconds after
// starting -- after `run`'s 2 s grace period, so `run` succeeds and the VM
// later ends by itself. Built static for musl by `tests/node_agent.rs`, like
// http_app.rs.
use std::io::Write;
use std::{thread, time::Duration};

fn main() {
    println!("EXIT_LATER_STARTED");
    std::io::stdout().flush().expect("flush marker");
    thread::sleep(Duration::from_secs(4));
    println!("EXIT_LATER_DONE");
    std::io::stdout().flush().expect("flush marker");
}
