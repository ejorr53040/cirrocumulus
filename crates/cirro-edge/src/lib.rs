//! The Node's edge: an HTTP reverse proxy that sends each request to the App
//! whose route names the request's `Host` (ADR 0007). It knows nothing of
//! VMs: a [`Router`], the Node agent, says where a hostname goes.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

/// How long the edge waits to connect to an App's VM before answering 502.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where requests for a hostname go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Proxy to the App's VM at this address.
    Upstream(SocketAddr),
    /// No App has this hostname: 404.
    NotFound,
    /// The App exists but can't take requests now: 503, saying why.
    Unavailable(String),
}

/// Says where requests for a hostname go.
pub trait Router: Send + Sync + 'static {
    /// `host` is in lowercase, without a port or a trailing dot.
    fn resolve(&self, host: &str) -> impl Future<Output = Resolution> + Send;
}

type Body = BoxBody<Bytes, hyper::Error>;

/// Serves HTTP on `listener` until the task is dropped, sending each
/// request where `router` says.
pub async fn serve<R: Router>(listener: TcpListener, router: Arc<R>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // Out of file descriptors, say: retrying at once would spin.
                warn!("edge accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let router = router.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let router = router.clone();
                async move { Ok::<_, hyper::Error>(handle(req, router.as_ref(), peer).await) }
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                debug!("edge connection from {peer}: {e}");
            }
        });
    }
}

async fn handle<R: Router>(req: Request<Incoming>, router: &R, peer: SocketAddr) -> Response<Body> {
    let Some(host) = host_of(&req) else {
        return text(
            StatusCode::BAD_REQUEST,
            "the request names no host\n".into(),
        );
    };
    match router.resolve(&host).await {
        Resolution::NotFound => text(
            StatusCode::NOT_FOUND,
            format!("no App on this Node has the hostname {host}\n"),
        ),
        Resolution::Unavailable(why) => text(StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")),
        Resolution::Upstream(upstream) => match proxy(req, &host, upstream, peer).await {
            Ok(response) => response,
            Err(e) => {
                warn!(host, %upstream, "proxy: {e}");
                text(
                    StatusCode::BAD_GATEWAY,
                    format!("the App at {host} didn't answer: {e}\n"),
                )
            }
        },
    }
}

/// The request's hostname, from `Host` (or the absolute-form URI's
/// authority), in lowercase and without a port or a trailing dot.
fn host_of(req: &Request<Incoming>) -> Option<String> {
    let raw = match req.uri().authority() {
        Some(authority) => authority.host().to_string(),
        None => {
            let value = req.headers().get(header::HOST)?.to_str().ok()?;
            let authority: hyper::http::uri::Authority = value.parse().ok()?;
            authority.host().to_string()
        }
    };
    let host = raw.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Sends `req` to `upstream` on a connection of its own and returns its
/// response, both without hop-by-hop headers.
async fn proxy(
    req: Request<Incoming>,
    host: &str,
    upstream: SocketAddr,
    peer: SocketAddr,
) -> Result<Response<Body>, String> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(upstream))
        .await
        .map_err(|_| format!("connecting to it timed out after {CONNECT_TIMEOUT:?}"))?
        .map_err(|e| format!("connect: {e}"))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!("upstream connection: {e}");
        }
    });

    let (mut parts, body) = req.into_parts();
    remove_hop_by_hop(&mut parts.headers);
    parts.version = Version::HTTP_11;
    parts.uri = parts
        .uri
        .path_and_query()
        .map_or_else(|| Uri::from_static("/"), |pq| Uri::from(pq.clone()));
    if let Ok(value) = HeaderValue::from_str(host) {
        parts.headers.insert(header::HOST, value.clone());
        parts
            .headers
            .insert(HeaderName::from_static("x-forwarded-host"), value);
    }
    // The edge is the first proxy a request meets, so whatever a client
    // put in X-Forwarded-For is its own claim: replaced, not appended to.
    if let Ok(value) = HeaderValue::from_str(&peer.ip().to_string()) {
        parts
            .headers
            .insert(HeaderName::from_static("x-forwarded-for"), value);
    }
    parts.headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static("http"),
    );

    let response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(|e| format!("send the request: {e}"))?;
    let (mut parts, body) = response.into_parts();
    remove_hop_by_hop(&mut parts.headers);
    Ok(Response::from_parts(parts, body.boxed()))
}

/// Headers that describe one connection, not the message, so never pass a
/// proxy (RFC 9110 7.6.1): the fixed set plus any that `Connection` names.
fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
}

fn text(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("build an edge response")
}
