//! A typed client over the Firecracker API, spoken over its Unix domain
//! socket -- Firecracker has no SDK of its own, so this *is* the client
//! (RESEARCH.md M3). A boot mirrors the `PUT` sequence
//! `scripts/step0/run_plain.sh`'s curl calls already proved works by hand:
//! `/machine-config` -> `/boot-source` -> `/drives/rootfs` ->
//! `/actions {InstanceStart}`. A park pauses the VM (`PATCH /vm`) and
//! writes a snapshot (`/snapshot/create`); a wake loads one into a fresh
//! Firecracker (`/snapshot/load`).

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::Mode;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;

/// A failed call to the Firecracker API: the request it was making and what
/// went wrong, whether the socket was unreachable or the API refused it.
#[derive(Debug)]
pub(crate) struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The most of an error response the client holds in memory: a VMM that has
/// been taken over mustn't fill the agent's with an error that never ends.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// How long Firecracker has to answer a request, and one that writes or
/// reads a whole guest's memory.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What `boot` needs to bring a VM up, matching the fields
/// `run_plain.sh` passes to `/machine-config`, `/boot-source` and
/// `/drives/rootfs`.
pub(crate) struct BootConfig {
    pub(crate) kernel_image_path: PathBuf,
    pub(crate) boot_args: String,
    pub(crate) rootfs_path: PathBuf,
    pub(crate) vcpu_count: u8,
    pub(crate) mem_size_mib: u32,
}

/// One Firecracker instance's API socket, at `socket` beneath `dir`. Every
/// request gets a connection of its own, made with [`connect_beneath`].
pub(crate) struct Client {
    dir: PathBuf,
    socket: &'static str,
}

impl Client {
    pub(crate) fn new(dir: impl Into<PathBuf>, socket: &'static str) -> Self {
        Self {
            dir: dir.into(),
            socket,
        }
    }

    /// Sends one `PUT` request with a JSON body to a path on the API, per
    /// <https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/actions.md>.
    async fn put(&self, path: &str, body: serde_json::Value) -> Result<(), Error> {
        self.send(Method::PUT, path, body).await
    }

    /// Sends one request and waits for its answer, but not for good: the
    /// socket is the VMM's own, and a VMM that has been taken over can
    /// accept a request and never answer it.
    async fn send(&self, method: Method, path: &str, body: serde_json::Value) -> Result<(), Error> {
        let limit = if path.starts_with("/snapshot/") {
            SNAPSHOT_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        tokio::time::timeout(limit, self.send_unbounded(method, path, body))
            .await
            .unwrap_or_else(|_| {
                Err(Error(format!(
                    "firecracker API: {path} went unanswered for {limit:?}"
                )))
            })
    }

    async fn send_unbounded(
        &self,
        method: Method,
        path: &str,
        body: serde_json::Value,
    ) -> Result<(), Error> {
        let body_bytes = serde_json::to_vec(&body).expect("serialize request body");

        let request = Request::builder()
            .method(method.clone())
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body_bytes)))
            .expect("build request");

        let stream = connect_beneath(&self.dir, self.socket)
            .await
            .map_err(|e| Error(format!("firecracker API socket: {e}")))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| Error(format!("firecracker API: {e}")))?;
        // Dropping the set aborts the connection, so one whose request timed
        // out doesn't live on holding the VMM's socket.
        let mut connection_task = tokio::task::JoinSet::new();
        connection_task.spawn(connection);
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| Error(format!("firecracker API: {e}")))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body_bytes = Limited::new(response.into_body(), MAX_ERROR_BODY)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        Err(Error(format!(
            "firecracker API: {method} {path} -> {status}: {}",
            String::from_utf8_lossy(&body_bytes)
        )))
    }

    /// Attaches a host tap device (already created and configured) to the
    /// VM as a network interface, per RESEARCH.md M3 slice 2. Must be called before `boot`
    /// -- like `/drives/rootfs`, `/network-interfaces/{iface_id}` is static
    /// device config the API only accepts pre-boot.
    pub(crate) async fn attach_tap(
        &self,
        iface_id: &str,
        host_dev_name: &str,
    ) -> Result<(), Error> {
        self.put(
            &format!("/network-interfaces/{iface_id}"),
            serde_json::json!({
                "iface_id": iface_id,
                "host_dev_name": host_dev_name,
            }),
        )
        .await
    }

    /// Adds a vsock device whose host end is a Unix socket at `uds_path`
    /// (inside the jail, when jailed). Pre-boot only, like `attach_tap`.
    pub(crate) async fn attach_vsock(&self, guest_cid: u32, uds_path: &str) -> Result<(), Error> {
        self.put(
            "/vsock",
            serde_json::json!({
                "guest_cid": guest_cid,
                "uds_path": uds_path,
            }),
        )
        .await
    }

    /// Presses Ctrl-Alt-Del in the guest (`SendCtrlAltDel`), which
    /// guest-init turns into SIGTERM for its command. Post-boot only.
    pub(crate) async fn send_ctrl_alt_del(&self) -> Result<(), Error> {
        self.put(
            "/actions",
            serde_json::json!({"action_type": "SendCtrlAltDel"}),
        )
        .await
    }

    /// Pauses the guest's vCPUs, as a snapshot needs. Post-boot only.
    pub(crate) async fn pause(&self) -> Result<(), Error> {
        self.set_vm_state("Paused").await
    }

    /// Lets a paused guest's vCPUs run again (Firecracker's `Resumed`
    /// state; nothing to do with waking a parked VM).
    pub(crate) async fn resume(&self) -> Result<(), Error> {
        self.set_vm_state("Resumed").await
    }

    async fn set_vm_state(&self, state: &str) -> Result<(), Error> {
        self.send(Method::PATCH, "/vm", serde_json::json!({ "state": state }))
            .await
    }

    /// Writes a full snapshot of a paused VM: its device state to `vmstate`
    /// and its guest memory to `mem` (paths as Firecracker sees them, so
    /// inside the jail).
    pub(crate) async fn create_snapshot(&self, vmstate: &str, mem: &str) -> Result<(), Error> {
        self.put(
            "/snapshot/create",
            serde_json::json!({
                "snapshot_type": "Full",
                "snapshot_path": vmstate,
                "mem_file_path": mem,
            }),
        )
        .await
    }

    /// Restores a snapshot [`Client::create_snapshot`] wrote into this fresh
    /// Firecracker, which must have had no other configuration, and resumes
    /// it. The memory file is mapped private, so the guest's writes never
    /// reach it. Every device the snapshot names must be where it was: the
    /// drive's file, the tap, the vsock socket's directory.
    pub(crate) async fn load_snapshot(&self, vmstate: &str, mem: &str) -> Result<(), Error> {
        self.put(
            "/snapshot/load",
            serde_json::json!({
                "snapshot_path": vmstate,
                "mem_backend": { "backend_type": "File", "backend_path": mem },
                "resume_vm": true,
            }),
        )
        .await
    }

    /// Drives the boot sequence: machine config, boot source, root drive,
    /// then `InstanceStart`. Other static device config (network, vsock)
    /// is pre-boot only too, so callers attach it ([`Client::attach_tap`],
    /// [`Client::attach_vsock`]) before calling this.
    pub(crate) async fn boot(&self, config: &BootConfig) -> Result<(), Error> {
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

/// Connects to the Unix socket at `socket` beneath `dir`, through no link
/// and no `..`. `dir` is a jail's root, which its VMM owns: a connect by
/// path, as root, would follow a link the VMM left there to another VM's
/// socket or any on the host, and checking the path first leaves a race.
pub(crate) async fn connect_beneath(dir: &Path, socket: &str) -> std::io::Result<UnixStream> {
    let dir = nix::fcntl::open(
        dir,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let found = nix::fcntl::openat2(
        &dir,
        socket,
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS),
    )?;
    // Linux has no connectat, but the fd's link in /proc names the inode
    // opened above, whatever the VMM has put at the path since.
    UnixStream::connect(format!("/proc/self/fd/{}", found.as_raw_fd())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The API socket is the VMM's own, so a VMM that has been taken over
    /// can accept a request and never answer it. A stop that waited on it
    /// for good could never go on to kill the VMM.
    #[tokio::test(start_paused = true)]
    async fn a_firecracker_that_never_answers_is_an_error_not_a_hang() {
        let dir = crate::test_util::tempdir("cirro-fc-silent");
        let socket = dir.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let silent = tokio::spawn(async move {
            let _held = listener.accept().await;
            std::future::pending::<()>().await
        });

        let client = Client::new(&dir, "api.sock");
        let asked = client.send_ctrl_alt_del();
        let answer = tokio::time::timeout(Duration::from_secs(3600), asked).await;

        assert!(
            matches!(answer, Ok(Err(_))),
            "a request Firecracker never answers should fail on its own: {answer:?}"
        );
        silent.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Connecting through the socket's `O_PATH` fd reaches the socket.
    #[tokio::test]
    async fn a_socket_beneath_the_jail_root_is_reached() {
        let jail = crate::test_util::tempdir("cirro-fc-beneath");
        std::fs::create_dir(jail.join("run")).unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(jail.join("run/firecracker.socket")).unwrap();
        listener.set_nonblocking(true).unwrap();

        connect_beneath(&jail, "run/firecracker.socket")
            .await
            .unwrap();

        listener.accept().unwrap();
        std::fs::remove_dir_all(jail).unwrap();
    }

    /// A taken-over VMM owns its jail's root, so it can put a link to
    /// another VM's socket, or the directory it's in, where its own `run`
    /// or `v.sock` should be. Nothing sent through either may reach it.
    #[tokio::test]
    async fn a_link_under_the_jail_root_is_refused_and_reaches_nothing() {
        let dir = crate::test_util::tempdir("cirro-fc-link");
        let neighbour_run = dir.join("neighbour/run");
        std::fs::create_dir_all(&neighbour_run).unwrap();
        let neighbour = neighbour_run.join("firecracker.socket");
        let listener = std::os::unix::net::UnixListener::bind(&neighbour).unwrap();
        listener.set_nonblocking(true).unwrap();
        let jail = dir.join("jail");
        std::fs::create_dir(&jail).unwrap();
        std::os::unix::fs::symlink(&neighbour_run, jail.join("run")).unwrap();
        std::os::unix::fs::symlink(&neighbour, jail.join("v.sock")).unwrap();

        let api = Client::new(&jail, "run/firecracker.socket")
            .send_ctrl_alt_del()
            .await;
        assert!(
            api.is_err(),
            "a link in place of `run` should be refused: {api:?}"
        );
        for socket in ["v.sock", "../neighbour/run/firecracker.socket"] {
            let found = connect_beneath(&jail, socket).await;
            assert!(found.is_err(), "{socket} should be refused: {found:?}");
        }

        let reached = listener.accept();
        assert!(
            matches!(&reached, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "nothing should reach the neighbour's socket: {reached:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
