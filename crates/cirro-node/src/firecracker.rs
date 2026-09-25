//! A typed client over the Firecracker API, spoken over its Unix domain
//! socket -- Firecracker has no SDK of its own, so this *is* the client
//! (RESEARCH.md M3). Mirrors the `PUT` sequence
//! `scripts/step0/run_plain.sh`'s curl calls already proved works by hand:
//! `/machine-config` -> `/boot-source` -> `/drives/rootfs` ->
//! `/actions {InstanceStart}`.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client as HyperClient;
use hyperlocal::{UnixClientExt, Uri as UnixUri};
use std::path::{Path, PathBuf};

/// A non-2xx response from the Firecracker API, or a transport failure
/// talking to it at all.
#[derive(Debug)]
pub enum Error {
    /// Couldn't even reach the API socket (e.g. Firecracker isn't up yet,
    /// or the path is wrong).
    Connect(String),
    /// The API accepted the connection but rejected the request.
    Request {
        path: String,
        status: StatusCode,
        body: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "connect to firecracker API socket: {e}"),
            Error::Request { path, status, body } => {
                write!(f, "PUT {path} -> {status}: {body}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// What `boot` needs to bring a VM up, matching the fields
/// `run_plain.sh` passes to `/machine-config`, `/boot-source` and
/// `/drives/rootfs`.
pub struct BootConfig {
    pub kernel_image_path: PathBuf,
    pub boot_args: String,
    pub rootfs_path: PathBuf,
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
}

/// A connection to one Firecracker instance's API socket. Cheap to clone
/// (the underlying `hyper_util` client is itself a cheap handle), but
/// there's only ever one Firecracker process per socket path, so callers
/// typically just make one and use it for that VM's whole lifetime.
#[derive(Clone)]
pub struct Client {
    inner: HyperClient<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: PathBuf,
}

impl Client {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            inner: HyperClient::unix(),
            socket_path: socket_path.into(),
        }
    }

    /// Sends one `PUT` request with a JSON body to a path on the API, per
    /// https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/actions.md.
    pub async fn put(&self, path: &str, body: serde_json::Value) -> Result<(), Error> {
        let uri: hyper::Uri = UnixUri::new(&self.socket_path, path).into();
        let body_bytes = serde_json::to_vec(&body).expect("serialize request body");

        let request = Request::builder()
            .method(Method::PUT)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body_bytes)))
            .expect("build request");

        let response = self
            .inner
            .request(request)
            .await
            .map_err(|e| Error::Connect(format!("{e:?}")))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body_bytes = response
            .into_body()
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        Err(Error::Request {
            path: path.to_string(),
            status,
            body: String::from_utf8_lossy(&body_bytes).into_owned(),
        })
    }

    /// Attaches a host tap device (already created and configured, e.g. via
    /// `cirro_node::network::create_persistent_tap`) to the VM as a network
    /// interface, per RESEARCH.md M3 slice 2. Must be called before `boot`
    /// -- like `/drives/rootfs`, `/network-interfaces/{iface_id}` is static
    /// device config the API only accepts pre-boot.
    pub async fn attach_tap(&self, iface_id: &str, host_dev_name: &str) -> Result<(), Error> {
        self.put(
            &format!("/network-interfaces/{iface_id}"),
            serde_json::json!({
                "iface_id": iface_id,
                "host_dev_name": host_dev_name,
            }),
        )
        .await
    }

    /// Drives the boot sequence: machine config, boot source, root drive,
    /// then `InstanceStart`. Static device config (network, vsock, ...)
    /// has to happen between the root drive and `InstanceStart` too, once
    /// callers need it -- this slice only covers what a plain boot needs.
    pub async fn boot(&self, config: &BootConfig) -> Result<(), Error> {
        self.put(
            "/machine-config",
            serde_json::json!({
                "vcpu_count": config.vcpu_count,
                "mem_size_mib": config.mem_size_mib,
            }),
        )
        .await?;

        self.put(
            "/boot-source",
            serde_json::json!({
                "kernel_image_path": path_str(&config.kernel_image_path),
                "boot_args": config.boot_args,
            }),
        )
        .await?;

        self.put(
            "/drives/rootfs",
            serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": path_str(&config.rootfs_path),
                "is_root_device": true,
                "is_read_only": false,
            }),
        )
        .await?;

        self.put(
            "/actions",
            serde_json::json!({"action_type": "InstanceStart"}),
        )
        .await?;

        Ok(())
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str()
        .unwrap_or_else(|| panic!("path is not valid UTF-8: {}", p.display()))
}
