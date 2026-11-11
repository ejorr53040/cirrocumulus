//! The `cirro-proto` HTTP+JSON API on the agent's socket: routing each
//! request to the agent, and every response's shape.

use super::*;
use http_body_util::{LengthLimitError, Limited};

pub(super) type ApiResponse = Response<Full<Bytes>>;

impl Agent {
    pub(super) async fn handle(
        self: Arc<Self>,
        req: Request<Incoming>,
        cred: UCred,
    ) -> ApiResponse {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        // Path names are validated here, once, so no handler echoes an
        // unvalidated string back. `run_vm` validates the name in its body.
        if let ["vms", name, ..] = segments.as_slice()
            && let Err(e) = validate_name("VM", name)
        {
            return json(e.0, &ErrorBody { error: e.1 });
        }
        if let ["machines", name, ..] = segments.as_slice()
            && let Err(e) = validate_name("Machine", name)
        {
            return json(e.0, &ErrorBody { error: e.1 });
        }
        if let ["vms", name, action] = segments.as_slice()
            && self.machines.has_app(name)
        {
            let cant = match *action {
                "logs" => "it has no log of it".to_string(),
                action => format!("it can't {action} it"),
            };
            let error = format!(
                "{name:?} is a Machine-backed App: Cirrocumulus doesn't run what serves it, so \
                 {cant}; `cirro rm {name}` takes it off the edge"
            );
            return json(StatusCode::CONFLICT, &ErrorBody { error });
        }
        let result = match (&method, segments.as_slice()) {
            (&Method::GET, ["machine-apps"]) => Ok(json(StatusCode::OK, &self.machines.apps())),
            (&Method::POST, ["machine-apps"]) => match read_json::<MachineAppRequest>(req).await {
                Ok(request) => self
                    .run_on_machine(request)
                    .map(|app| json(StatusCode::CREATED, &app)),
                Err(e) => Err(e),
            },
            (&Method::GET, ["stats"]) => Ok(json(StatusCode::OK, &self.stats())),
            (&Method::GET, ["ca"]) => self.ca_pem(),
            (&Method::GET, ["vms"]) => {
                let all = query_param(&query, "all") == Some("true");
                Ok(json(StatusCode::OK, &self.list(all)))
            }
            (&Method::POST, ["vms"]) => match read_json::<RunRequest>(req).await {
                Ok(run) => self
                    .run_vm(run, cred)
                    .await
                    .map(|vm| json(StatusCode::CREATED, &vm)),
                Err(e) => Err(e),
            },
            (&Method::POST, ["vms", name, "stop"]) => {
                let name = name.to_string();
                match read_json::<StopRequest>(req).await {
                    Ok(stop) => self
                        .stop_vm(&name, stop)
                        .await
                        .map(|vm| json(StatusCode::OK, &vm)),
                    Err(e) => Err(e),
                }
            }
            (&Method::POST, ["vms", name, "park"]) => {
                let name = name.to_string();
                self.park_vm(name).await.map(|vm| json(StatusCode::OK, &vm))
            }
            (&Method::POST, ["vms", name, "wake"]) => {
                let name = name.to_string();
                self.wake_vm(name)
                    .await
                    .map(|vm| json(StatusCode::CREATED, &vm))
            }
            (&Method::GET, ["vms", name, "logs"]) => {
                let offset = query_param(&query, "offset")
                    .map(|o| o.parse::<u64>())
                    .transpose()
                    .map_err(|_| bad_request("offset must be a byte count"));
                offset.and_then(|offset| self.logs(name, offset.unwrap_or(0)))
            }
            (&Method::DELETE, ["vms", name]) => self.remove(name).map(|()| no_content()),
            (&Method::GET, ["ssh-key"]) => self.machines.public_key().map(|key| {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(Full::new(Bytes::from(key)))
                    .expect("build key response")
            }),
            (&Method::GET, ["machines"]) => Ok(json(StatusCode::OK, &self.machines.list())),
            (&Method::POST, ["machines", "scan"]) => match read_json::<MachineScan>(req).await {
                Ok(scan) => self
                    .machines
                    .scan(&scan.ssh)
                    .await
                    .map(|host_key| json(StatusCode::OK, &MachineHostKey { host_key })),
                Err(e) => Err(e),
            },
            (&Method::POST, ["machines"]) => match read_json::<AddMachine>(req).await {
                Ok(add) => self
                    .machines
                    .add(add)
                    .await
                    .map(|info| json(StatusCode::CREATED, &info)),
                Err(e) => Err(e),
            },
            (&Method::DELETE, ["machines", name]) => {
                self.machines.remove(name).await.map(|()| no_content())
            }
            _ => Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("no such endpoint: {method} {path}"),
            )),
        };
        result.unwrap_or_else(|ApiError(status, error)| json(status, &ErrorBody { error }))
    }

    /// The CA certificate the HTTPS edge's certificates come from.
    fn ca_pem(&self) -> Result<ApiResponse, ApiError> {
        let ca = self.ca.as_ref().ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "this Node agent serves no HTTPS edge, so has no CA; start it with --https".into(),
            )
        })?;
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/x-pem-file")
            .body(Full::new(Bytes::from(ca.cert_pem().to_string())))
            .expect("build CA response"))
    }
}

pub(super) fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

pub(super) struct ApiError(pub(super) StatusCode, pub(super) String);

pub(super) fn bad_request(message: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.to_string())
}

pub(super) fn internal(message: String) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// The most of a request body the agent holds in memory: far more than a
/// request with a long command and environment needs.
const MAX_BODY: usize = 1 << 20;

pub(super) async fn read_json<T: serde::de::DeserializeOwned>(
    req: Request<Incoming>,
) -> Result<T, ApiError> {
    let body = Limited::new(req.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| match e.downcast_ref::<LengthLimitError>() {
            Some(_) => ApiError(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the request body is over {MAX_BODY} bytes"),
            ),
            None => bad_request(&format!("read request body: {e}")),
        })?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|e| bad_request(&format!("invalid request body: {e}")))
}

pub(super) fn no_content() -> ApiResponse {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()))
        .expect("build empty response")
}

pub(super) fn json(status: StatusCode, body: &impl serde::Serialize) -> ApiResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(body).expect("serialize response"),
        )))
        .expect("build response")
}
