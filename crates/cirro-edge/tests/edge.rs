//! The HTTP edge at its public seam, `cirro_edge::serve`, with a stub
//! [`Router`] in place of the Node agent and a plain TCP listener as the
//! App.

use cirro_edge::{Lease, Resolution, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Sends every hostname to one upstream.
struct To(SocketAddr);

impl Router for To {
    async fn resolve(&self, _host: &str) -> Resolution {
        Resolution::Upstream(self.0, Lease::new(()))
    }
}

/// An edge in front of `upstream`, and the address it listens on.
async fn edge(upstream: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(cirro_edge::serve(listener, Arc::new(To(upstream))));
    address
}

/// Reads one request's head from `stream`, lowercased.
async fn read_head(stream: &mut TcpStream) -> String {
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
    let edge = edge(app.local_addr().unwrap()).await;

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
    let edge = edge(app.local_addr().unwrap()).await;

    let mut client = TcpStream::connect(edge).await.unwrap();
    client.write_all(b"G").await.unwrap();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(20), client.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "the edge still holds a stalled connection");
}
