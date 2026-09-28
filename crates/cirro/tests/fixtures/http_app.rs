// M3 fixture: the smallest app worth `curl`ing at a VM address. Answers
// every connection on port 8080 with a fixed body, then closes it. Built
// static for musl by `tests/node_agent.rs` (not part of the cargo
// workspace, like scripts/step0/fixtures), so the guest needs no libc.
use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let listener = TcpListener::bind("0.0.0.0:8080").expect("bind 0.0.0.0:8080");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        let body = "hello from cirro\n";
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}
