// M4 fixture: prints the identity guest-init started it with -- `WHOAMI uid=<uid> gid=<gid>`,
// `WHOAMI cwd=<dir>`, one `WHOAMI env <KEY=VALUE>` per variable -- then
// `WHOAMI_DONE`, and stays running so `run`'s grace period never mistakes it
// for a failed start. Built static for musl by `tests/node_agent.rs`.
use std::io::Write;
use std::{thread, time::Duration};

fn main() {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let first_id = |field: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|ids| ids.split_whitespace().next())
            .unwrap_or("?")
            .to_string()
    };
    println!("WHOAMI uid={} gid={}", first_id("Uid:"), first_id("Gid:"));
    println!("WHOAMI cwd={}", std::env::current_dir().expect("cwd").display());
    for (key, value) in std::env::vars() {
        println!("WHOAMI env {key}={value}");
    }
    println!("WHOAMI_DONE");
    std::io::stdout().flush().expect("flush");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
