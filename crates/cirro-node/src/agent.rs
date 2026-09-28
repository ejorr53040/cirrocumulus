//! The Node agent (ADR 0002): the long-running, privileged `cirro node
//! agent` process that owns every VM on this Node. It serves the
//! `cirro-proto` HTTP+JSON API on a Unix socket, allocates VM addresses and
//! drives [`crate::vm`] for each request.
//!
//! State lives in memory (M3 slice 1). Until persistence and restart
//! reconcile land, the agent tears down every VM it owns when it stops, so
//! a stopped agent never strands VMs it can no longer find.

use crate::vm::{self, Vm, VmSpec};
use bytes::Bytes;
use cirro_proto::{ErrorBody, RunRequest, StopRequest, VmInfo};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nix::sys::stat::{Mode, umask};
use std::collections::BTreeMap;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};

/// A Node subnet such as `10.77.0.0/24`. `.1` is the Node's own address;
/// VM addresses are handed out from `.2` up.
#[derive(Debug, Clone, Copy)]
pub struct Subnet {
    network: u32,
    prefix_len: u8,
}

impl FromStr for Subnet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, len) = s
            .split_once('/')
            .ok_or_else(|| format!("{s:?} is not a CIDR like 10.77.0.0/24"))?;
        let addr: Ipv4Addr = addr.parse().map_err(|e| format!("{s:?}: {e}"))?;
        let prefix_len: u8 = len.parse().map_err(|e| format!("{s:?}: {e}"))?;
        if !(16..=30).contains(&prefix_len) {
            return Err(format!("{s:?}: prefix length must be between 16 and 30"));
        }
        let mask = u32::MAX << (32 - prefix_len);
        Ok(Subnet {
            network: u32::from(addr) & mask,
            prefix_len,
        })
    }
}

impl Subnet {
    fn node_address(self) -> Ipv4Addr {
        Ipv4Addr::from(self.network + 1)
    }

    /// Every address a VM may hold, lowest first.
    fn vm_addresses(self) -> impl Iterator<Item = Ipv4Addr> {
        let broadcast = self.network | (u32::MAX >> self.prefix_len);
        (self.network + 2..broadcast).map(Ipv4Addr::from)
    }
}

/// Everything `cirro node agent` is started with.
pub struct Config {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    /// The group allowed to use the socket (`root:<group>`, `0660`): a name,
    /// or a numeric gid.
    pub socket_group: String,
    pub subnet: Subnet,
    pub firecracker: PathBuf,
    pub jailer: PathBuf,
    pub kernel: PathBuf,
}

enum Entry {
    /// `Vm::start` is running; the entry reserves the name and VM address.
    Starting,
    Running {
        info: VmInfo,
        vm: Vm,
    },
}

struct Agent {
    node: vm::NodeConfig,
    subnet: Subnet,
    vms: Mutex<BTreeMap<String, (Ipv4Addr, Entry)>>,
    /// Set once shutdown begins, so no new VM starts after the agent has
    /// begun tearing VMs down.
    shutting_down: AtomicBool,
}

/// Runs the agent until SIGTERM or SIGINT, then tears down every VM and
/// removes the socket, and the jail tree and parent cgroup once empty.
pub async fn run(config: Config) -> io::Result<()> {
    std::fs::create_dir_all(&config.state_dir)?;
    let agent = Arc::new(Agent {
        node: vm::NodeConfig {
            firecracker: config.firecracker,
            jailer: config.jailer,
            kernel: config.kernel,
            jail_base: config.state_dir.join("jail"),
            state_dir: config.state_dir.clone(),
            node_address: config.subnet.node_address(),
        },
        subnet: config.subnet,
        vms: Mutex::new(BTreeMap::new()),
        shutting_down: AtomicBool::new(false),
    });

    let gid = resolve_group(&config.socket_group)?;
    if let Some(dir) = config.socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(&config.socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // Bind under a umask that leaves the socket root-only until it's handed
    // to its group, so it's never briefly open to everyone.
    let old_umask = umask(Mode::from_bits_truncate(0o177));
    let bound = UnixListener::bind(&config.socket);
    umask(old_umask);
    let listener = bound?;
    std::os::unix::fs::chown(&config.socket, Some(0), Some(gid))?;
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o660))?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(conn) => conn,
                    Err(e) => {
                        eprintln!("cirro node: accept: {e}");
                        continue;
                    }
                };
                let agent = agent.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let agent = agent.clone();
                        async move { Ok::<_, hyper::Error>(agent.handle(req).await) }
                    });
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        eprintln!("cirro node: connection: {e}");
                    }
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    let _ = std::fs::remove_file(&config.socket);
    agent.shutdown().await;
    Ok(())
}

type ApiResponse = Response<Full<Bytes>>;

impl Agent {
    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> ApiResponse {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let result = match (&method, segments.as_slice()) {
            (&Method::GET, ["vms"]) => Ok(json(StatusCode::OK, &self.list())),
            (&Method::POST, ["vms"]) => match read_json::<RunRequest>(req).await {
                Ok(run) => self.run_vm(run).await,
                Err(e) => Err(e),
            },
            (&Method::POST, ["vms", name, "stop"]) => {
                let name = name.to_string();
                match read_json::<StopRequest>(req).await {
                    Ok(stop) => self.stop_vm(&name, stop).await,
                    Err(e) => Err(e),
                }
            }
            _ => Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("no such endpoint: {method} {path}"),
            )),
        };
        result.unwrap_or_else(|ApiError(status, error)| json(status, &ErrorBody { error }))
    }

    fn list(&self) -> Vec<VmInfo> {
        let vms = self.vms.lock().unwrap();
        vms.values()
            .filter_map(|(_, entry)| match entry {
                Entry::Running { info, .. } => Some(info.clone()),
                Entry::Starting => None,
            })
            .collect()
    }

    async fn run_vm(self: Arc<Self>, run: RunRequest) -> Result<ApiResponse, ApiError> {
        if run.command.is_empty() {
            return Err(bad_request("no command given to run in the VM"));
        }
        if !run.rootfs.is_absolute() {
            return Err(bad_request("the rootfs path must be absolute"));
        }
        let vm_address = {
            let mut vms = self.vms.lock().unwrap();
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the Node agent is shutting down".into(),
                ));
            }
            if vms.contains_key(&run.name) {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("a VM named {:?} already exists", run.name),
                ));
            }
            let held: Vec<Ipv4Addr> = vms.values().map(|(addr, _)| *addr).collect();
            let vm_address = self
                .subnet
                .vm_addresses()
                .find(|a| !held.contains(a))
                .ok_or_else(|| {
                    ApiError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "the Node subnet has no free VM addresses".into(),
                    )
                })?;
            vms.insert(run.name.clone(), (vm_address, Entry::Starting));
            vm_address
        };

        let spec = VmSpec {
            vm_address,
            rootfs: run.rootfs,
            mem_mib: run.mem_mib,
            vcpus: run.vcpus,
            command: run.command,
        };
        // Started on its own task, so the start runs to completion (success,
        // or a full unwind) even if the client disconnects and hyper drops
        // this request's future.
        let name = run.name;
        tokio::spawn(async move { self.start_vm(name, spec).await })
            .await
            .unwrap_or_else(|e| {
                Err(ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("starting the VM panicked: {e}"),
                ))
            })
    }

    /// Runs `Vm::start` for a name and VM address already reserved as
    /// `Entry::Starting`, then records the result.
    async fn start_vm(&self, name: String, spec: VmSpec) -> Result<ApiResponse, ApiError> {
        let vm_address = spec.vm_address;
        match Vm::start(&self.node, &spec).await {
            Ok(vm) => {
                let info = VmInfo {
                    name: name.clone(),
                    vm_address,
                    mem_mib: spec.mem_mib,
                    vcpus: spec.vcpus,
                    started_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                };
                self.vms.lock().unwrap().insert(
                    name,
                    (
                        vm_address,
                        Entry::Running {
                            info: info.clone(),
                            vm,
                        },
                    ),
                );
                Ok(json(StatusCode::CREATED, &info))
            }
            Err(e) => {
                self.vms.lock().unwrap().remove(&name);
                Err(ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("starting VM {name:?} failed: {e}"),
                ))
            }
        }
    }

    async fn stop_vm(&self, name: &str, stop: StopRequest) -> Result<ApiResponse, ApiError> {
        if !stop.force {
            return Err(ApiError(
                StatusCode::NOT_IMPLEMENTED,
                "graceful stop isn't implemented yet; use --force".into(),
            ));
        }
        let vm = {
            let mut vms = self.vms.lock().unwrap();
            match vms.get(name) {
                None => {
                    return Err(ApiError(
                        StatusCode::NOT_FOUND,
                        format!("no VM named {name:?}"),
                    ));
                }
                Some((_, Entry::Starting)) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("VM {name:?} is still starting"),
                    ));
                }
                Some((_, Entry::Running { .. })) => match vms.remove(name) {
                    Some((_, Entry::Running { vm, .. })) => vm,
                    _ => unreachable!("entry was just matched as running"),
                },
            }
        };
        vm.kill().await;
        Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Full::new(Bytes::new()))
            .expect("build empty response"))
    }

    /// Tears down every VM, including ones still starting: each start
    /// finishes (bounded by its own timeouts) as a running VM to kill, or
    /// unwinds itself.
    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        loop {
            let (running, any_starting) = {
                let mut vms = self.vms.lock().unwrap();
                let running_names: Vec<String> = vms
                    .iter()
                    .filter(|(_, (_, entry))| matches!(entry, Entry::Running { .. }))
                    .map(|(name, _)| name.clone())
                    .collect();
                let running: Vec<Vm> = running_names
                    .iter()
                    .filter_map(|name| match vms.remove(name) {
                        Some((_, Entry::Running { vm, .. })) => Some(vm),
                        _ => None,
                    })
                    .collect();
                (running, !vms.is_empty())
            };
            for vm in running {
                vm.kill().await;
            }
            if !any_starting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Only succeed once empty, so a jail still in use is never removed.
        let _ = std::fs::remove_dir(self.node.jail_base.join("firecracker"));
        let _ = std::fs::remove_dir(&self.node.jail_base);
        let _ = std::fs::remove_dir(vm::parent_cgroup());
    }
}

fn resolve_group(group: &str) -> io::Result<u32> {
    if let Ok(gid) = group.parse() {
        return Ok(gid);
    }
    match nix::unistd::Group::from_name(group)? {
        Some(g) => Ok(g.gid.as_raw()),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no group named {group:?} for the agent socket"),
        )),
    }
}

struct ApiError(StatusCode, String);

fn bad_request(message: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.to_string())
}

async fn read_json<T: serde::de::DeserializeOwned>(req: Request<Incoming>) -> Result<T, ApiError> {
    let body = req
        .into_body()
        .collect()
        .await
        .map_err(|e| bad_request(&format!("read request body: {e}")))?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|e| bad_request(&format!("invalid request body: {e}")))
}

fn json(status: StatusCode, body: &impl serde::Serialize) -> ApiResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(body).expect("serialize response"),
        )))
        .expect("build response")
}
