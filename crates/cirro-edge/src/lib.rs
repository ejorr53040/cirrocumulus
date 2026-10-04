//! The Node's edge: an HTTP reverse proxy that sends each request to the App
//! whose route names the request's `Host` (ADR 0007). It knows nothing of
//! VMs: a [`Router`], the Node agent, says where a hostname goes.

pub mod acme;
mod fs;
pub mod tls;

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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tracing::{debug, warn};

/// How long a client has to finish its TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the edge waits to connect to an App's VM before answering 502.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where requests for a hostname go.
#[derive(Debug)]
pub enum Resolution {
    /// Proxy to the App's VM at this address, holding the lease until the
    /// response has been sent.
    Upstream(SocketAddr, Lease),
    /// No App has this hostname: 404.
    NotFound,
    /// The App exists but can't take requests now: 503, saying why.
    Unavailable(String),
}

/// Whatever a [`Router`] wants held while a request it routed is in flight:
/// dropped once the response has been sent, or the request has failed.
pub struct Lease {
    _held: Box<dyn Send + Sync>,
}

impl Lease {
    pub fn new(held: impl Send + Sync + 'static) -> Lease {
        Lease {
            _held: Box::new(held),
        }
    }
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Lease")
    }
}

/// Says where requests for a hostname go.
pub trait Router: Send + Sync + 'static {
    /// `host` is in lowercase, without a port or a trailing dot.
    fn resolve(&self, host: &str) -> impl Future<Output = Resolution> + Send;

    /// The answer to an ACME HTTP-01 challenge for `token`, while one is
    /// under way: the HTTP edge serves it at
    /// `/.well-known/acme-challenge/<token>` for any hostname.
    fn acme_challenge(&self, token: &str) -> Option<String> {
        let _ = token;
        None
    }
}

type Body = BoxBody<Bytes, hyper::Error>;

/// Serves HTTP on `listener` until the task is dropped, sending each
/// request where `router` says.
pub async fn serve<R: Router>(listener: TcpListener, router: Arc<R>) {
    accept(listener, router, None).await
}

/// Serves HTTPS on `listener` with `config` until the task is dropped,
/// sending each request where `router` says.
pub async fn serve_tls<R: Router>(
    listener: TcpListener,
    router: Arc<R>,
    config: Arc<ServerConfig>,
) {
    accept(listener, router, Some(TlsAcceptor::from(config))).await
}

async fn accept<R: Router>(listener: TcpListener, router: Arc<R>, tls: Option<TlsAcceptor>) {
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
        let tls = tls.clone();
        tokio::spawn(async move {
            let Some(tls) = tls else {
                return serve_connection(stream, router, peer, Scheme::Http).await;
            };
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)).await {
                Ok(Ok(stream)) => {
                    // The resolver only answers a client with an SNI name.
                    let sni = stream.get_ref().1.server_name().unwrap_or_default();
                    let scheme = Scheme::Https {
                        sni: sni.trim_end_matches('.').to_ascii_lowercase(),
                    };
                    serve_connection(stream, router, peer, scheme).await
                }
                Ok(Err(e)) => debug!("TLS handshake with {peer}: {e}"),
                Err(_) => debug!("TLS handshake with {peer} timed out"),
            }
        });
    }
}

/// What a client connection arrived over.
#[derive(Clone)]
enum Scheme {
    Http,
    /// With the hostname the client named in its TLS handshake, which its
    /// certificate was chosen for.
    Https {
        sni: String,
    },
}

impl Scheme {
    /// Its `X-Forwarded-Proto` value.
    fn proto(&self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https { .. } => "https",
        }
    }
}

/// Serves the requests on one client connection.
async fn serve_connection<R, S>(stream: S, router: Arc<R>, peer: SocketAddr, scheme: Scheme)
where
    R: Router,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |req| {
        let (router, scheme) = (router.clone(), scheme.clone());
        async move { Ok::<_, hyper::Error>(handle(req, router.as_ref(), peer, &scheme).await) }
    });
    if let Err(e) = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        debug!("edge connection from {peer}: {e}");
    }
}

async fn handle<R: Router>(
    req: Request<Incoming>,
    router: &R,
    peer: SocketAddr,
    scheme: &Scheme,
) -> Response<Body> {
    let Some(host) = host_of(&req) else {
        return text(
            StatusCode::BAD_REQUEST,
            "the request names no host\n".into(),
        );
    };
    if let Scheme::Http = scheme
        && let Some(token) = req
            .uri()
            .path()
            .strip_prefix("/.well-known/acme-challenge/")
        && let Some(answer) = router.acme_challenge(token)
    {
        return text(StatusCode::OK, answer);
    }
    // A connection's certificate was chosen for one hostname; a request on
    // it for another would borrow that certificate (RFC 9110 15.5.20).
    if let Scheme::Https { sni } = scheme
        && *sni != host
    {
        return text(
            StatusCode::MISDIRECTED_REQUEST,
            format!("this connection is for {sni}, not {host}; connect again for {host}\n"),
        );
    }
    match router.resolve(&host).await {
        Resolution::NotFound => text(
            StatusCode::NOT_FOUND,
            format!("no App on this Node has the hostname {host}\n"),
        ),
        Resolution::Unavailable(why) => text(StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")),
        Resolution::Upstream(upstream, lease) => {
            match proxy(req, &host, upstream, peer, scheme.proto(), lease).await {
                Ok(response) => response,
                Err(e) => {
                    warn!(host, %upstream, "proxy: {e}");
                    text(
                        StatusCode::BAD_GATEWAY,
                        format!("the App at {host} didn't answer: {e}\n"),
                    )
                }
            }
        }
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
    proto: &'static str,
    lease: Lease,
) -> Result<Response<Body>, String> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, connect(upstream))
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
        HeaderValue::from_static(proto),
    );

    let response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(|e| format!("send the request: {e}"))?;
    let (mut parts, body) = response.into_parts();
    remove_hop_by_hop(&mut parts.headers);
    // The body owns the lease, so it lasts until the body is sent or
    // dropped.
    let body = body.map_frame(move |frame| {
        // Named, so the closure captures the lease rather than ignoring it.
        let _held = &lease;
        frame
    });
    Ok(Response::from_parts(parts, body.boxed()))
}

/// Connects to `upstream`, trying again while it refuses: an App that has
/// just started or woken may be a moment from accepting.
async fn connect(upstream: SocketAddr) -> std::io::Result<TcpStream> {
    loop {
        match TcpStream::connect(upstream).await {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => return result,
        }
    }
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
