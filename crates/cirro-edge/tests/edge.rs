//! The HTTP edge at its public seam, `cirro_edge::serve`, with a stub
//! [`Router`] in place of the Node agent and a plain TCP or Unix listener
//! as the App.

use cirro_edge::{Lease, Resolution, Router, Upstream};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener};

/// Sends every hostname to one upstream.
struct To(SocketAddr);

impl Router for To {
    async fn resolve(&self, _host: &str) -> Resolution {
        Resolution::Upstream(Upstream::Tcp(self.0), Lease::new(()))
    }

    fn routes(&self, _host: &str) -> bool {
        true
    }
}

/// Sends every hostname to one Unix socket.
struct ToSocket(PathBuf);

impl Router for ToSocket {
    async fn resolve(&self, _host: &str) -> Resolution {
        Resolution::Upstream(Upstream::Unix(self.0.clone()), Lease::new(()))
    }

    fn routes(&self, _host: &str) -> bool {
        true
    }
}

/// An edge in front of `router`'s upstream, and the address it listens on.
async fn edge(router: impl Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(cirro_edge::serve(listener, Arc::new(router), None));
    address
}

/// Reads one request's head from `stream`, lowercased.
async fn read_head(stream: &mut (impl AsyncRead + Unpin)) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap().to_ascii_lowercase()
}

#[tokio::test]
async fn the_app_sees_the_real_client_address_and_no_hop_by_hop_headers() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = edge(To(app.local_addr().unwrap())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client
        .write_all(
            b"GET / HTTP/1.1\r\nHost: web.test\r\nX-Forwarded-For: 1.2.3.4\r\n\
              Connection: x-secret\r\nX-Secret: hidden\r\n\r\n",
        )
        .await
        .unwrap();
    let (mut upstream, _) = app.accept().await.unwrap();
    let head = read_head(&mut upstream).await;

    assert!(head.contains("x-forwarded-for: 127.0.0.1\r\n"), "{head}");
    assert!(
        !head.contains("1.2.3.4"),
        "the client's claim was kept: {head}"
    );
    assert!(
        !head.contains("x-secret"),
        "a hop-by-hop header passed: {head}"
    );
    assert!(head.contains("host: web.test\r\n"), "{head}");
    assert!(head.contains("x-forwarded-proto: http\r\n"), "{head}");
}

#[tokio::test]
async fn a_client_that_never_finishes_its_request_is_disconnected() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = edge(To(app.local_addr().unwrap())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client.write_all(b"G").await.unwrap();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(20), client.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "the edge still holds a stalled connection");
}

/// A Machine-backed App's forward is a Unix socket, which the edge proxies
/// to as it does to a VM's address.
#[tokio::test]
async fn an_app_on_a_unix_socket_gets_the_request_and_answers_it() {
    let path = std::env::temp_dir().join(format!("cirro-edge-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let app = UnixListener::bind(&path).unwrap();
    let edge = edge(ToSocket(path.clone())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: pi.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (mut upstream, _) = app.accept().await.unwrap();
    let head = read_head(&mut upstream).await;
    assert!(head.contains("host: pi.test\r\n"), "{head}");
    upstream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
        .await
        .unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut answer))
        .await
        .expect("the edge never finished answering")
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer:?}");
    assert!(answer.ends_with("hi"), "{answer:?}");
}

/// The client-IP headers other than `X-Forwarded-*` are the client's own
/// claims too, so the App must not see them.
#[tokio::test]
async fn the_app_never_sees_a_client_claimed_ip_header() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = edge(To(app.local_addr().unwrap())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client
        .write_all(
            b"GET / HTTP/1.1\r\nHost: web.test\r\nX-Real-IP: 10.0.0.1\r\n\
              True-Client-IP: 10.0.0.2\r\nCF-Connecting-IP: 10.0.0.3\r\n\
              X-Client-IP: 10.0.0.4\r\nForwarded: for=10.0.0.5\r\n\
              X-Forwarded-Port: 1\r\n\r\n",
        )
        .await
        .unwrap();
    let (mut upstream, _) = app.accept().await.unwrap();
    let head = read_head(&mut upstream).await;

    for claim in ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4", "10.0.0.5"] {
        assert!(!head.contains(claim), "the client's {claim} passed: {head}");
    }
    assert!(!head.contains("x-forwarded-port"), "{head}");
    assert!(!head.contains("x-real-ip"), "{head}");
    assert!(!head.contains("forwarded:"), "{head}");
}

/// CONNECT would make the edge a tunnel; ADR 0007 has none, so the App never
/// sees the request.
#[tokio::test]
async fn connect_is_refused_and_the_app_never_sees_it() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = edge(To(app.local_addr().unwrap())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client
        .write_all(
            b"CONNECT web.test:443 HTTP/1.1\r\nHost: web.test:443\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut answer))
        .await
        .expect("the edge never answered CONNECT")
        .unwrap();
    assert!(answer.starts_with("HTTP/1.1 405"), "{answer:?}");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), app.accept())
            .await
            .is_err(),
        "the App was contacted for CONNECT"
    );
}

/// A client that stops mid-body must not hold its App's connection: the edge
/// gives up on the body after an idle gap, and the App's side closes.
#[tokio::test]
async fn a_client_that_stalls_mid_body_is_cut_off_and_frees_the_app() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = edge(To(app.local_addr().unwrap())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: web.test\r\nContent-Length: 100\r\n\r\n0123456789")
        .await
        .unwrap();
    let (mut upstream, _) = app.accept().await.unwrap();
    read_head(&mut upstream).await;

    let mut rest = Vec::new();
    let patience = Duration::from_secs(60);
    tokio::time::timeout(patience, upstream.read_to_end(&mut rest))
        .await
        .expect("the edge kept the App's connection open after the client stalled")
        .unwrap();
    assert_eq!(rest, b"0123456789");
    tokio::time::timeout(patience, client.read_to_end(&mut Vec::new()))
        .await
        .expect("the edge kept the stalled client's connection open")
        .unwrap();
}

/// Idle time between body frames is what's bounded, so a slow but steady
/// upload gets through whole.
#[tokio::test]
async fn a_steady_upload_reaches_the_app_whole() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = edge(To(app.local_addr().unwrap())).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: web.test\r\nContent-Length: 6\r\n\r\nab")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.write_all(b"cdef").await.unwrap();
    let (mut upstream, _) = app.accept().await.unwrap();
    read_head(&mut upstream).await;
    let mut body = [0u8; 6];
    upstream.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, b"abcdef");
}
