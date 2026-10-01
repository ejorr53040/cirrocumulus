// M6 fixture: traffic the guest starts itself. Opens a TCP connection to
// its `ip:port` argument every 100 ms and prints `DIAL <n> OPEN` or
// `DIAL <n> FAILED <error>`, so a guest woken from a snapshot shows whether
// it can reach out before anything has reached in to it.
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::{thread, time::Duration};

fn main() {
    let target: SocketAddr = std::env::args()
        .nth(1)
        .expect("a target ip:port")
        .parse()
        .expect("target is ip:port");
    for n in 1u64.. {
        match TcpStream::connect_timeout(&target, Duration::from_millis(500)) {
            Ok(_) => println!("DIAL {n} OPEN"),
            Err(e) => println!("DIAL {n} FAILED {e}"),
        }
        let _ = std::io::stdout().flush();
        thread::sleep(Duration::from_millis(100));
    }
}
