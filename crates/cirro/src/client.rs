//! The CLI's side of the Node agent API (`cirro-proto`): one HTTP request
//! per call over the agent's Unix socket.

use bytes::Bytes;
use cirro_proto::ErrorBody;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderMap;
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, Uri};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;

/// Sends one request and decodes a JSON response body, `None` when the
/// body is empty. Any failure comes back as the message the CLI prints.
pub(crate) async fn call<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
) -> Result<Option<T>, String> {
    let (_, body) = send(socket, method, path, body).await?;
    if body.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("unexpected response from the Node agent: {e}"))
}

/// Sends one request and returns a successful response's headers and raw
/// body.
pub(crate) async fn send(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
) -> Result<(HeaderMap, Bytes), String> {
    let bytes = match body {
        Some(b) => Bytes::from(serde_json::to_vec(b).expect("serialize request")),
        None => Bytes::new(),
    };
    let request = Request::builder()
        .method(method)
        .uri(hyper::Uri::from(Uri::new(socket, path)))
        .header("content-type", "application/json")
        .body(Full::new(bytes))
        .expect("build request");
    let response = Client::unix()
        .request(request)
        .await
        .map_err(|e| connect_error(socket, &e))?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("reading the Node agent's response: {e}"))?
        .to_bytes();
    if !status.is_success() {
        return Err(match serde_json::from_slice::<ErrorBody>(&body) {
            Ok(e) => e.error,
            Err(_) => format!("the Node agent answered {status}"),
        });
    }
    Ok((headers, body))
}

/// Explains a failure to reach the agent, calling out the common case of a
/// user outside the socket's group.
fn connect_error(socket: &Path, error: &(dyn std::error::Error + 'static)) -> String {
    let mut source = Some(error);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::PermissionDenied
        {
            return format!(
                "permission denied on the Node agent's socket {}: ask the Node's operator \
                 to add you to its group (cirro by default)",
                socket.display()
            );
        }
        source = e.source();
    }
    format!(
        "can't reach the Node agent at {} ({error}); is it running?",
        socket.display()
    )
}
