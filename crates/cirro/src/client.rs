//! The CLI's side of the Node agent API (`cirro-proto`): one HTTP request
//! per command over the agent's Unix socket.

use bytes::Bytes;
use cirro_proto::ErrorBody;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, Uri};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;

/// Sends one request and returns the response body on success. Any failure
/// comes back as the message the CLI prints.
pub(crate) async fn call<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
) -> Result<Option<T>, String> {
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
    let response = Client::unix().request(request).await.map_err(|e| {
        format!(
            "can't reach the Node agent at {} ({e}); is it running, and are you in its group?",
            socket.display()
        )
    })?;
    let status = response.status();
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
    if body.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("unexpected response from the Node agent: {e}"))
}
