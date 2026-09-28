//! The Node agent (ADR 0002): the long-running, privileged `cirro node
//! agent` process that owns every VM on this Node. It serves the
//! `cirro-proto` HTTP+JSON API on a Unix socket, allocates VM addresses and
//! drives [`crate::vm`] for each request.
//!
//! Each running VM is owned by its own supervisor task ([`Vm::supervise`]).
//! When the VM ends, for whatever reason, the task records it as an Ended VM
//! whose console log stays in the state dir until `rm` or name reuse.
//!
//! State lives in memory (M3). Until persistence and restart reconcile land
//! (#9), the agent tears down every VM it owns when it stops, so a stopped
//! agent never strands VMs it can no longer find, and Ended VMs are
//! forgotten along with their logs.

use crate::vm::{self, Stop, Vm, VmSpec};
use bytes::Bytes;
use cirro_proto::{EndReason, Ended, ErrorBody, RunRequest, StopRequest, VM_STATE_HEADER, VmInfo};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nix::sys::stat::{Mode, umask};
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

/// How long a graceful stop waits for the VM to end before killing it,
/// unless the request says otherwise.
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

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

/// One name's record in the registry.
enum Entry {
    /// `Vm::start` is running; the entry reserves the name and VM address.
    Starting { vm_address: Ipv4Addr },
    /// Owned by a supervisor task, which `stops` reaches. `ended` turns true
    /// once the task has recorded the VM as ended.
    Running {
        info: VmInfo,
        stops: mpsc::Sender<Stop>,
        ended: watch::Receiver<bool>,
    },
    /// History only: holds no host state and no VM address.
    Ended { info: VmInfo },
}

impl Entry {
    fn vm_address(&self) -> Option<Ipv4Addr> {
        match self {
            Entry::Starting { vm_address } => Some(*vm_address),
            Entry::Running { info, .. } => info.vm_address,
            Entry::Ended { .. } => None,
        }
    }
}

struct Agent {
    node: vm::NodeConfig,
    subnet: Subnet,
    /// Console logs, one per name, kept with the Ended VM record.
    logs_dir: PathBuf,
    vms: Mutex<BTreeMap<String, Entry>>,
    /// Set once shutdown begins, so no new VM starts after the agent has
    /// begun tearing VMs down.
    shutting_down: AtomicBool,
}

/// Runs the agent until SIGTERM or SIGINT, then tears down every VM and
/// removes the socket, console logs, and the jail tree and parent cgroup
/// once empty.
pub async fn run(config: Config) -> io::Result<()> {
    let logs_dir = config.state_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let agent = Arc::new(Agent {
        node: vm::NodeConfig {
            firecracker: config.firecracker,
            jailer: config.jailer,
            kernel: config.kernel,
            jail_base: config.state_dir.join("jail"),
            node_address: config.subnet.node_address(),
        },
        subnet: config.subnet,
        logs_dir,
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
        let query = req.uri().query().unwrap_or("").to_string();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let result = match (&method, segments.as_slice()) {
            (&Method::GET, ["vms"]) => {
                let all = query_param(&query, "all") == Some("true");
                Ok(json(StatusCode::OK, &self.list(all)))
            }
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
            (&Method::GET, ["vms", name, "logs"]) => {
                let offset = query_param(&query, "offset")
                    .map(|o| o.parse::<u64>())
                    .transpose()
                    .map_err(|_| bad_request("offset must be a byte count"));
                offset.and_then(|offset| self.logs(name, offset.unwrap_or(0)))
            }
            (&Method::DELETE, ["vms", name]) => self.remove(name),
            _ => Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("no such endpoint: {method} {path}"),
            )),
        };
        result.unwrap_or_else(|ApiError(status, error)| json(status, &ErrorBody { error }))
    }

    fn list(&self, all: bool) -> Vec<VmInfo> {
        let vms = self.vms.lock().unwrap();
        vms.values()
            .filter_map(|entry| match entry {
                Entry::Running { info, .. } => Some(info.clone()),
                Entry::Ended { info } if all => Some(info.clone()),
                _ => None,
            })
            .collect()
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.log"))
    }

    async fn run_vm(self: Arc<Self>, run: RunRequest) -> Result<ApiResponse, ApiError> {
        validate_name(&run.name)?;
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
            match vms.get(&run.name) {
                Some(Entry::Starting { .. } | Entry::Running { .. }) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("a VM named {:?} already exists", run.name),
                    ));
                }
                // Reusing an Ended VM's name replaces its record and log.
                Some(Entry::Ended { .. }) => {
                    vms.remove(&run.name);
                    let _ = std::fs::remove_file(self.log_path(&run.name));
                }
                None => {}
            }
            let held: Vec<Ipv4Addr> = vms.values().filter_map(Entry::vm_address).collect();
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
            vms.insert(run.name.clone(), Entry::Starting { vm_address });
            vm_address
        };

        let spec = VmSpec {
            vm_address,
            rootfs: run.rootfs,
            mem_mib: run.mem_mib,
            vcpus: run.vcpus,
            command: run.command,
            console_log: self.log_path(&run.name),
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
    /// `Entry::Starting`, then hands a started VM to its supervisor task.
    /// A VM that fails to start leaves no record and no log.
    async fn start_vm(
        self: Arc<Self>,
        name: String,
        spec: VmSpec,
    ) -> Result<ApiResponse, ApiError> {
        let vm = match Vm::start(&self.node, &spec).await {
            Ok(vm) => vm,
            Err(e) => {
                self.vms.lock().unwrap().remove(&name);
                let _ = std::fs::remove_file(&spec.console_log);
                return Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("VM {name:?} failed to start: {e}"),
                ));
            }
        };
        let info = VmInfo {
            name: name.clone(),
            vm_address: Some(spec.vm_address),
            mem_mib: spec.mem_mib,
            vcpus: spec.vcpus,
            started_at: now(),
            ended: None,
        };
        let (stops_tx, stops_rx) = mpsc::channel(4);
        let (ended_tx, ended_rx) = watch::channel(false);
        self.vms.lock().unwrap().insert(
            name.clone(),
            Entry::Running {
                info: info.clone(),
                stops: stops_tx,
                ended: ended_rx,
            },
        );
        let agent = self.clone();
        tokio::spawn(async move {
            let reason = vm.supervise(stops_rx).await;
            agent.record_end(&name, reason);
            let _ = ended_tx.send(true);
        });
        Ok(json(StatusCode::CREATED, &info))
    }

    /// Turns a running VM's record into an Ended VM's, once its host state
    /// is gone.
    fn record_end(&self, name: &str, reason: EndReason) {
        let mut vms = self.vms.lock().unwrap();
        if let Some(Entry::Running { mut info, .. }) = vms.remove(name) {
            info.vm_address = None;
            info.ended = Some(Ended { at: now(), reason });
            vms.insert(name.to_string(), Entry::Ended { info });
        }
    }

    async fn stop_vm(&self, name: &str, stop: StopRequest) -> Result<ApiResponse, ApiError> {
        let (stops, mut ended) = {
            let vms = self.vms.lock().unwrap();
            match vms.get(name) {
                None => return Err(no_such_vm(name)),
                Some(Entry::Starting { .. }) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("VM {name:?} is still starting"),
                    ));
                }
                Some(Entry::Ended { .. }) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("VM {name:?} has already ended"),
                    ));
                }
                Some(Entry::Running { stops, ended, .. }) => (stops.clone(), ended.clone()),
            }
        };
        let request = if stop.force {
            Stop::Force
        } else {
            Stop::Graceful {
                timeout: stop
                    .timeout_secs
                    .map_or(DEFAULT_STOP_TIMEOUT, Duration::from_secs),
            }
        };
        // If the supervisor is already gone, the VM is ending anyway.
        let _ = stops.send(request).await;
        let _ = ended.wait_for(|ended| *ended).await;
        match self.vms.lock().unwrap().get(name) {
            Some(Entry::Ended { info }) => Ok(json(StatusCode::OK, info)),
            _ => Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("VM {name:?} ended, but its record is gone"),
            )),
        }
    }

    fn logs(&self, name: &str, offset: u64) -> Result<ApiResponse, ApiError> {
        let state = match self.vms.lock().unwrap().get(name) {
            None => return Err(no_such_vm(name)),
            Some(Entry::Ended { .. }) => "ended",
            Some(_) => "running",
        };
        let mut bytes = Vec::new();
        if let Ok(mut file) = std::fs::File::open(self.log_path(name)) {
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_to_end(&mut bytes))
                .map_err(|e| {
                    ApiError(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("read the console log: {e}"),
                    )
                })?;
        }
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; charset=utf-8")
            .header(VM_STATE_HEADER, state)
            .body(Full::new(Bytes::from(bytes)))
            .expect("build logs response"))
    }

    fn remove(&self, name: &str) -> Result<ApiResponse, ApiError> {
        let mut vms = self.vms.lock().unwrap();
        match vms.get(name) {
            None => Err(no_such_vm(name)),
            Some(Entry::Ended { .. }) => {
                vms.remove(name);
                let _ = std::fs::remove_file(self.log_path(name));
                Ok(Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Full::new(Bytes::new()))
                    .expect("build empty response"))
            }
            Some(_) => Err(ApiError(
                StatusCode::CONFLICT,
                format!("VM {name:?} is running; stop it before removing it"),
            )),
        }
    }

    /// Tears down every VM, including ones still starting: each start
    /// finishes (bounded by its own timeouts) as a running VM to stop, or
    /// unwinds itself.
    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        loop {
            let (running, any_starting) = {
                let vms = self.vms.lock().unwrap();
                let running: Vec<_> = vms
                    .values()
                    .filter_map(|entry| match entry {
                        Entry::Running { stops, ended, .. } => Some((stops.clone(), ended.clone())),
                        _ => None,
                    })
                    .collect();
                let any_starting = vms.values().any(|e| matches!(e, Entry::Starting { .. }));
                (running, any_starting)
            };
            if running.is_empty() && !any_starting {
                break;
            }
            for (stops, mut ended) in running {
                let _ = stops.send(Stop::Force).await;
                let _ = ended.wait_for(|ended| *ended).await;
            }
            if any_starting {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // Ended VMs are only in memory, so their logs go with them.
        let _ = std::fs::remove_dir_all(&self.logs_dir);
        // Only succeed once empty, so a jail still in use is never removed.
        let _ = std::fs::remove_dir(self.node.jail_base.join("firecracker"));
        let _ = std::fs::remove_dir(&self.node.jail_base);
        let _ = std::fs::remove_dir(vm::parent_cgroup());
    }
}

/// VM names are 1-32 characters of lowercase letters, digits and hyphens,
/// so they're safe in file names, URLs and log lines.
fn validate_name(name: &str) -> Result<(), ApiError> {
    let valid = (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(bad_request(&format!(
            "invalid VM name {name:?}: a name is 1-32 characters of lowercase letters, \
             digits and hyphens"
        )))
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

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

struct ApiError(StatusCode, String);

fn bad_request(message: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.to_string())
}

fn no_such_vm(name: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("no VM named {name:?}"))
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
