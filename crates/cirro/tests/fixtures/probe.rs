// M3 fixture: tries a TCP connection to each `ip:port` argument from inside
// the VM and prints one line per target -- `PROBE <target> OPEN`,
// `PROBE <target> REFUSED` (reachable, nothing listening) or
// `PROBE <target> FAILED <error>` (e.g. dropped, so it timed out) -- then
// `PROBE_DONE`. Stays running afterwards, so `run`'s grace period never
// mistakes a quick probe for a failed start. Built static for musl by
// `tests/node_agent.rs`, like http_app.rs.
use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, TcpStream};
use std::{thread, time::Duration};

fn main() {
    for target in std::env::args().skip(1) {
        let addr: SocketAddr = target.parse().expect("target is ip:port");
        let result = match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(_) => "OPEN".to_string(),
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => "REFUSED".to_string(),
            Err(e) => format!("FAILED {e}"),
        };
        println!("PROBE {target} {result}");
    }
    println!("PROBE_DONE");
    std::io::stdout().flush().expect("flush probe results");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
