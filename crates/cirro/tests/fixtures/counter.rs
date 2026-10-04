// M6 fixture: state that only memory holds. Answers each connection on port
// 8080 with how many it has answered so far (`count 1`, `count 2`, ...), so
// a guest restored from a snapshot carries on counting where a rebooted one
// would start again at 1. `GET /slow` is answered after 4 s, for a request
// that stays in flight.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    let listener = TcpListener::bind("0.0.0.0:8080").expect("bind 0.0.0.0:8080");
    println!("COUNTER_FIXTURE_LISTENING");
    let mut count = 0u64;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // The whole request, or closing with some of it unread resets the
        // connection instead of delivering the answer.
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        if request.starts_with(b"GET /slow ") {
            std::thread::sleep(Duration::from_secs(4));
        }
        count += 1;
        let body = format!("count {count}\n");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}
