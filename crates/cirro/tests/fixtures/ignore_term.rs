// M3 fixture: a command that ignores SIGTERM and never exits, so a graceful
// `cirro stop` has to fall back to killing the VM once its timeout runs out.
// Built static for musl by `tests/node_agent.rs`, like http_app.rs.
use std::io::Write;
use std::{thread, time::Duration};

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

const SIGTERM: i32 = 15;
const SIG_IGN: usize = 1;

fn main() {
    unsafe {
        signal(SIGTERM, SIG_IGN);
    }
    println!("IGNORE_TERM_STARTED");
    std::io::stdout().flush().expect("flush marker");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
