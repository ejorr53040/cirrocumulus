// M7 fixture (security audit SA-01): UDP from inside a VM, including from
// a source address the guest doesn't own.
//
// `udp listen <port>` prints `UDP_LISTENING`, then `UDP_FROM <ip:port>` for
// every datagram that arrives.
//
// `udp spoof <source-ip> <ip:port>...` sends a datagram from `<source-ip>`
// to each target every 100 ms, printing `UDP_SENT <n>` after each round
// (and `UDP_SEND_FAILED <target> <error>` for a send that failed). It binds
// the forged source with IP_TRANSPARENT (guest root has CAP_NET_ADMIN), so
// the guest kernel sends it as-is: what a malicious guest can always do. Built static for musl by `tests/node_agent.rs`; no crates,
// so the three libc calls are declared here.
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::FromRawFd;
use std::{thread, time::Duration};

#[repr(C)]
struct SockaddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

unsafe extern "C" {
    fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const i32, len: u32) -> i32;
    fn bind(fd: i32, addr: *const SockaddrIn, len: u32) -> i32;
}

const AF_INET: i32 = 2;
const SOCK_DGRAM: i32 = 2;
const IPPROTO_IP: i32 = 0;
const IP_TRANSPARENT: i32 = 19;

/// A UDP socket bound to `source`, whether or not the guest has it.
fn bind_any(source: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let last = std::io::Error::last_os_error;
    // SAFETY: plain syscalls on a fresh fd we own; `addr` and `on` outlive them.
    unsafe {
        let fd = socket(AF_INET, SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(last());
        }
        let on = 1;
        if setsockopt(fd, IPPROTO_IP, IP_TRANSPARENT, &on, 4) < 0 {
            return Err(last());
        }
        let addr = SockaddrIn {
            family: AF_INET as u16,
            port: 0,
            addr: source.octets(),
            zero: [0; 8],
        };
        if bind(fd, &addr, size_of::<SockaddrIn>() as u32) < 0 {
            return Err(last());
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["listen", port] => {
            let socket = UdpSocket::bind(("0.0.0.0", port.parse().expect("a port")))
                .expect("bind the listener");
            let _ = writeln!(out, "UDP_LISTENING");
            let mut buf = [0; 64];
            loop {
                let (_, from) = socket.recv_from(&mut buf).expect("recv");
                let _ = writeln!(out, "UDP_FROM {from}");
            }
        }
        ["spoof", source, targets @ ..] if !targets.is_empty() => {
            let targets: Vec<SocketAddr> = targets
                .iter()
                .map(|t| t.parse().expect("target is ip:port"))
                .collect();
            let socket = bind_any(source.parse().expect("source is an ip")).expect("bind source");
            for n in 0u64.. {
                for target in &targets {
                    if let Err(e) = socket.send_to(b"spoofed", target) {
                        let _ = writeln!(out, "UDP_SEND_FAILED {target} {e}");
                    }
                }
                let _ = writeln!(out, "UDP_SENT {n}");
                thread::sleep(Duration::from_millis(100));
            }
        }
        _ => panic!("usage: udp listen <port> | udp spoof <source-ip> <ip:port>..."),
    }
}
