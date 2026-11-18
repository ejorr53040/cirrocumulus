//! The Node's edge: an HTTP reverse proxy that sends each request to the App
//! whose route names the request's `Host` (ADR 0007). It knows nothing of
//! VMs: a [`Router`], the Node agent, says where a hostname goes.

pub mod acme;
mod fs;
pub mod tls;

use acme::Acme;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tls::NodeCa;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::Notify;
use tokio::time::{Instant, Sleep};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

/// How long a client has to finish its TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client has to send each request's headers, the first one
/// and each one after on a kept-alive connection.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the edge waits to connect to an App before answering 502.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a client may pause between request body frames: a stalled upload pins an App awake.
const CLIENT_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long an App may go silent (no request body frame, no response head): a hung App pins its lease.
const UPSTREAM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long an App's response may pause between body frames: a stopped App pins the client's connection.
const UPSTREAM_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where requests for a hostname go.
#[derive(Debug)]
pub enum Resolution {
    /// Proxy to the App at this upstream, holding the lease until the
    /// response has been sent.
    Upstream(Upstream, Lease),
    /// No App has this hostname: 404.
    NotFound,
    /// The App exists but can't take requests now: 503, saying why.
    Unavailable(String),
}

/// Where an App takes the requests the edge proxies to it.
#[derive(Debug)]
pub enum Upstream {
    /// A VM's address.
    Tcp(SocketAddr),
    /// A Unix socket, such as a Machine-backed App's forward, which only
    /// the agent can reach.
    Unix(PathBuf),
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

    /// Whether an App has `host`, without starting or waking anything: the
    /// HTTPS edge makes certificates only for hostnames that route.
    fn routes(&self, host: &str) -> bool;
}

/// What a body or the edge's own timeouts can fail with.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

type Body = BoxBody<Bytes, BoxError>;

/// Serves HTTP on `listener` until the task is dropped, sending each
/// request where `router` says. With `acme`, it also answers the ACME CA's
/// HTTP-01 challenges, at `/.well-known/acme-challenge/<token>` for any
/// hostname.
pub async fn serve<R: Router>(listener: TcpListener, router: Arc<R>, acme: Option<Arc<Acme>>) {
    accept(listener, router, None, acme).await
}

/// Serves HTTPS on `listener` until the task is dropped, sending each
/// request where `router` says, with a certificate for each hostname it
/// routes: from `acme` for a public hostname when there is one, else signed
/// by `ca`.
pub fn serve_tls<R: Router>(
    listener: TcpListener,
    router: Arc<R>,
    ca: Arc<NodeCa>,
    acme: Option<Arc<Acme>>,
) -> std::io::Result<impl Future<Output = ()> + Send> {
    let routes = router.clone();
    let config = tls::server_config(ca, acme, move |host| routes.routes(host))?;
    Ok(accept(
        listener,
        router,
        Some(TlsAcceptor::from(config)),
        None,
    ))
}

/// Whether `host` is a hostname in the form the edge compares them: dot-
/// separated labels of lowercase letters, digits and inner hyphens, with no
/// trailing dot, so it is also safe in a file name.
pub fn is_hostname(host: &str) -> bool {
    let label_ok = |label: &str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    host.len() <= 253 && host.split('.').all(label_ok)
}

/// `host` as the edge compares hostnames: lowercase, without a trailing dot.
fn normalize(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

async fn accept<R: Router>(
    listener: TcpListener,
    router: Arc<R>,
    tls: Option<TlsAcceptor>,
    acme: Option<Arc<Acme>>,
) {
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
        let acme = acme.clone();
        tokio::spawn(async move {
            let Some(tls) = tls else {
                return serve_connection(stream, router, peer, Scheme::Http { acme }).await;
            };
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)).await {
                Ok(Ok(stream)) => {
                    let sni = stream
                        .get_ref()
                        .1
                        .server_name()
                        .expect("the resolver only answers a client with an SNI name");
                    let scheme = Scheme::Https {
                        sni: normalize(sni),
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
enum Scheme {
    /// With the ACME CA whose challenges it answers, when there is one.
    Http { acme: Option<Arc<Acme>> },
    /// With the hostname the client named in its TLS handshake, which its
    /// certificate was chosen for.
    Https { sni: String },
}

impl Scheme {
    /// Its `X-Forwarded-Proto` value.
    fn proto(&self) -> &'static str {
        match self {
            Scheme::Http { .. } => "http",
            Scheme::Https { .. } => "https",
        }
    }
}

async fn serve_connection<R, S>(stream: S, router: Arc<R>, peer: SocketAddr, scheme: Scheme)
where
    R: Router,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let scheme = Arc::new(scheme);
    let service = service_fn(move |req| {
        let (router, scheme) = (router.clone(), scheme.clone());
        async move { Ok::<_, hyper::Error>(handle(req, router.as_ref(), peer, &scheme).await) }
    });
    // hyper skips its default header timeout without a timer, which would
    // let idle clients hold the agent's file descriptors forever.
    if let Err(e) = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
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
    // The edge proxies requests and never tunnels one (ADR 0007).
    if req.method() == Method::CONNECT {
        return text(
            StatusCode::METHOD_NOT_ALLOWED,
            "the edge doesn't tunnel CONNECT\n".into(),
        );
    }
    let Some(host) = host_of(&req) else {
        return text(
            StatusCode::BAD_REQUEST,
            "the request names no host\n".into(),
        );
    };
    if let Scheme::Http { acme: Some(acme) } = scheme
        && let Some(token) = req
            .uri()
            .path()
            .strip_prefix("/.well-known/acme-challenge/")
        && let Some(answer) = acme.challenge(token)
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
            match proxy(req, &host, &upstream, peer, scheme.proto(), lease).await {
                Ok(response) => response,
                Err(e) => {
                    warn!(host, ?upstream, "proxy: {e}");
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
    let host = match req.uri().authority() {
        Some(authority) => normalize(authority.host()),
        None => {
            let value = req.headers().get(header::HOST)?.to_str().ok()?;
            let authority: hyper::http::uri::Authority = value.parse().ok()?;
            normalize(authority.host())
        }
    };
    (!host.is_empty()).then_some(host)
}

/// Sends `req` to `upstream` on a connection of its own and returns its
/// response, both without hop-by-hop headers.
async fn proxy(
    req: Request<Incoming>,
    host: &str,
    upstream: &Upstream,
    peer: SocketAddr,
    proto: &'static str,
    lease: Lease,
) -> Result<Response<Body>, String> {
    match upstream {
        Upstream::Tcp(addr) => {
            let stream = connect(upstream, || TcpStream::connect(*addr)).await?;
            forward(stream, req, host, peer, proto, lease).await
        }
        Upstream::Unix(path) => {
            let stream = connect(upstream, || UnixStream::connect(path)).await?;
            forward(stream, req, host, peer, proto, lease).await
        }
    }
}

async fn forward<S>(
    stream: S,
    req: Request<Incoming>,
    host: &str,
    peer: SocketAddr,
    proto: &'static str,
    lease: Lease,
) -> Result<Response<Body>, String>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
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
    remove_client_ip_headers(&mut parts.headers);
    parts.version = Version::HTTP_11;
    parts.uri = parts
        .uri
        .path_and_query()
        .map_or_else(|| Uri::from_static("/"), |pq| Uri::from(pq.clone()));
    let value = HeaderValue::from_str(host).expect("a hostname the edge parsed is a header value");
    parts.headers.insert(header::HOST, value.clone());
    parts
        .headers
        .insert(HeaderName::from_static("x-forwarded-host"), value);
    // The edge is the first proxy a request meets, so whatever a client
    // put in X-Forwarded-For is its own claim: replaced, not appended to.
    let peer =
        HeaderValue::from_str(&peer.ip().to_string()).expect("an IP address is a header value");
    parts
        .headers
        .insert(HeaderName::from_static("x-forwarded-for"), peer);
    parts.headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static(proto),
    );

    let progress = Arc::new(Notify::new());
    let body = IdleTimeout::new(body, CLIENT_BODY_IDLE_TIMEOUT, Some(progress.clone()));
    let request = Request::from_parts(parts, body);
    let response = await_head(sender.send_request(request), &progress).await?;
    let (mut parts, body) = response.into_parts();
    remove_hop_by_hop(&mut parts.headers);
    let body = IdleTimeout::new(body, UPSTREAM_BODY_IDLE_TIMEOUT, None);
    // The body owns the lease, so it lasts until the body is sent or
    // dropped.
    let body = body.map_frame(move |frame| {
        // Named, so the closure captures the lease rather than ignoring it.
        let _held = &lease;
        frame
    });
    Ok(Response::from_parts(parts, body.boxed()))
}

/// Connects to `upstream` with `open`, trying again while it refuses: an
/// App that has just started or woken may be a moment from accepting.
async fn connect<S, F>(upstream: &Upstream, open: impl Fn() -> F) -> Result<S, String>
where
    F: Future<Output = std::io::Result<S>>,
{
    let refused_meanwhile = async {
        loop {
            match open().await {
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                result => return result,
            }
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT, refused_meanwhile)
        .await
        .map_err(|_| match upstream {
            Upstream::Tcp(addr) => format!(
                "nothing accepted a connection on port {} within {CONNECT_TIMEOUT:?}",
                addr.port()
            ),
            // The socket's path is the Node's own business, not the edge's
            // clients'.
            Upstream::Unix(_) => {
                format!("nothing accepted a connection within {CONNECT_TIMEOUT:?}")
            }
        })?
        .map_err(|e| format!("connect: {e}"))
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

/// Headers that name the client's own address or the proxies it came through,
/// which only the edge may set: the App gets the edge's word for them (SA-12).
fn remove_client_ip_headers(headers: &mut HeaderMap) {
    let claimed: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name.starts_with("x-forwarded-")
                || matches!(
                    name,
                    "forwarded"
                        | "x-real-ip"
                        | "true-client-ip"
                        | "cf-connecting-ip"
                        | "x-client-ip"
                )
        })
        .cloned()
        .collect();
    for name in claimed {
        headers.remove(name);
    }
}

/// Waits for the App's response head. The App may take as long as the request
/// body takes to arrive, so the deadline restarts with each body frame sent: it
/// gives up only on an App that has gone silent.
async fn await_head<F, T, E>(send: F, progress: &Notify) -> Result<T, String>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::pin!(send);
    let deadline = tokio::time::sleep(UPSTREAM_RESPONSE_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            head = &mut send => return head.map_err(|e| format!("send the request: {e}")),
            () = progress.notified() => {
                deadline.as_mut().reset(Instant::now() + UPSTREAM_RESPONSE_TIMEOUT);
            }
            () = &mut deadline => {
                return Err(format!("no response within {UPSTREAM_RESPONSE_TIMEOUT:?}"));
            }
        }
    }
}

/// A body that fails once `idle` passes without a frame, so a peer that stalls
/// mid-body can't hold its connection; a steady body of any length passes.
/// `progress`, when set, is notified of each frame.
struct IdleTimeout<B> {
    body: B,
    idle: Duration,
    timer: Pin<Box<Sleep>>,
    progress: Option<Arc<Notify>>,
}

impl<B: hyper::body::Body + Unpin> IdleTimeout<B> {
    fn new(body: B, idle: Duration, progress: Option<Arc<Notify>>) -> Self {
        Self {
            body,
            idle,
            timer: Box::pin(tokio::time::sleep(idle)),
            progress,
        }
    }
}

impl<B> hyper::body::Body for IdleTimeout<B>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(frame) => {
                if frame.is_some() {
                    this.timer.as_mut().reset(Instant::now() + this.idle);
                    if let Some(progress) = &this.progress {
                        progress.notify_one();
                    }
                }
                Poll::Ready(frame.map(|result| result.map_err(Into::into)))
            }
            Poll::Pending => match this.timer.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    Poll::Ready(Some(Err(
                        format!("no body frame for {:?}", this.idle).into()
                    )))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
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
