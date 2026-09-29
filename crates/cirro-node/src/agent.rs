//! The Node agent (ADR 0002): the long-running, privileged `cirro node
//! agent` process that owns every VM on this Node. It serves the
//! `cirro-proto` HTTP+JSON API on a Unix socket, allocates VM addresses and
//! drives [`crate::vm`] for each request.
//!
//! Each running VM is owned by its own supervisor task ([`Vm::supervise`]).
//! When the VM ends, for whatever reason, the task records it as an Ended VM
//! whose console log stays in the state dir until `rm` or name reuse.
//!
//! VMs outlive the agent (ADR 0002). Each VM is recorded in the state
//! database once it has started; the agent never stops VMs when it exits,
//! and a starting agent takes back every VM whose record still matches a
//! running process before it opens its socket.

use crate::egress;
use crate::rootfs_open;
use crate::state::{Record, Store};
use crate::subnet::Subnet;
use crate::vm::{self, Stop, Vm, VmSpec};
use bytes::Bytes;
use cirro_proto::{
    EndReason, Ended, ErrorBody, MAX_MEM_MIB, MAX_VCPUS, MIN_MEM_MIB, MIN_VCPUS, RunRequest,
    StopRequest, VM_STATE_HEADER, VmInfo,
};
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::net::unix::UCred;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

/// How long a graceful stop waits for the VM to end before killing it,
/// unless the request says otherwise.
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

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

/// One name's record in the registry. Every VM start gets its own console
/// log file (`log`), so a failed reuse of a name can't clobber the log of
/// the Ended VM it would have replaced.
enum Entry {
    /// `Vm::start` is running; the entry reserves the name and VM address.
    Starting { vm_address: Ipv4Addr, log: PathBuf },
    /// Owned by a supervisor task, which `stops` reaches. `ended` turns true
    /// once the task has recorded the VM as ended.
    Running {
        info: VmInfo,
        log: PathBuf,
        stops: mpsc::Sender<Stop>,
        ended: watch::Receiver<bool>,
    },
    /// History only: holds no host state and no VM address.
    Ended { info: VmInfo, log: PathBuf },
}

impl Entry {
    fn vm_address(&self) -> Option<Ipv4Addr> {
        match self {
            Entry::Starting { vm_address, .. } => Some(*vm_address),
            Entry::Running { info, .. } => info.vm_address,
            Entry::Ended { .. } => None,
        }
    }

    fn log(&self) -> &PathBuf {
        match self {
            Entry::Starting { log, .. } | Entry::Running { log, .. } | Entry::Ended { log, .. } => {
                log
            }
        }
    }
}

/// An Ended VM set aside while a new VM with its name starts: dropped if the
/// start succeeds, put back if it fails.
struct Replaced {
    info: VmInfo,
    log: PathBuf,
}

struct Agent {
    node: vm::NodeConfig,
    subnet: Subnet,
    /// Console logs, one per name, kept with the Ended VM record.
    logs_dir: PathBuf,
    vms: Mutex<BTreeMap<String, Entry>>,
    store: Mutex<Store>,
    /// Numbers each VM start's console log file.
    next_log: AtomicU64,
    /// Set once shutdown begins, so no new VM starts while the agent waits for
    /// the starts already under way to finish.
    shutting_down: AtomicBool,
}

/// Runs the agent until SIGTERM or SIGINT, then removes the socket. VMs,
/// their records and their console logs stay, and the jail tree and parent
/// cgroup go only if no VM is left in them. The Node's egress policy is
/// ensured first, and outlives the agent.
pub async fn run(config: Config) -> io::Result<()> {
    // Both refusals below run before the agent changes anything on the
    // Node: a live socket is checked first (the fastest way to tell this
    // start is a mistake), then the state dir's subnet.
    refuse_if_already_running(&config.socket)?;

    let logs_dir = config.state_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let store = Store::open(
        &config.state_dir.join("state.db"),
        &config.subnet.to_string(),
    )?;

    // Fail closed: no VM starts without the egress policy in place.
    let egress_iface = egress::default_route_iface();
    if egress_iface.is_none() {
        eprintln!("cirro node: no IPv4 default route, so VMs won't reach the internet");
    }
    egress::enable_ip_forward(&config.state_dir.join("ip_forward.before"))?;
    egress::ensure_node_policy(&config.subnet.to_string(), egress_iface.as_deref())
        .map_err(|e| io::Error::other(format!("apply the Node's egress policy: {e}")))?;
    let gid = resolve_group(&config.socket_group)?;
    let agent = Arc::new(Agent {
        node: vm::NodeConfig {
            firecracker: config.firecracker,
            jailer: config.jailer,
            kernel: config.kernel,
            jail_base: config.state_dir.join("jail"),
            node_address: config.subnet.node_address(),
            cirro_gid: gid,
        },
        subnet: config.subnet,
        logs_dir: logs_dir.clone(),
        vms: Mutex::new(BTreeMap::new()),
        store: Mutex::new(store),
        next_log: AtomicU64::new(next_log_number(&logs_dir)),
        shutting_down: AtomicBool::new(false),
    });
    agent.reconcile().await?;

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
                let cred = match stream.peer_cred() {
                    Ok(cred) => cred,
                    Err(e) => {
                        eprintln!("cirro node: read peer credentials: {e}");
                        continue;
                    }
                };
                let agent = agent.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let agent = agent.clone();
                        async move { Ok::<_, hyper::Error>(agent.handle(req, cred).await) }
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

/// Refuses to start if `socket` already has a live listener, telling that
/// apart from a socket file merely left behind by a crash (connecting to it
/// finds no one home). Any other error connecting -- wrong file type at
/// that path, a permissions problem -- is treated as unsafe to guess about
/// and also refused, rather than silently reused as if stale.
fn refuse_if_already_running(socket: &Path) -> io::Result<()> {
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => Err(io::Error::other(format!(
            "cirro node: agent already running at {}",
            socket.display()
        ))),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

type ApiResponse = Response<Full<Bytes>>;

impl Agent {
    async fn handle(self: Arc<Self>, req: Request<Incoming>, cred: UCred) -> ApiResponse {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        // Path names are validated here, once, so no handler echoes an
        // unvalidated string back. `run_vm` validates the name in its body.
        if let ["vms", name, ..] = segments.as_slice()
            && let Err(e) = validate_name(name)
        {
            return json(e.0, &ErrorBody { error: e.1 });
        }
        let result = match (&method, segments.as_slice()) {
            (&Method::GET, ["vms"]) => {
                let all = query_param(&query, "all") == Some("true");
                Ok(json(StatusCode::OK, &self.list(all)))
            }
            (&Method::POST, ["vms"]) => match read_json::<RunRequest>(req).await {
                Ok(run) => self.run_vm(run, cred).await,
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
                Entry::Ended { info, .. } if all => Some(info.clone()),
                _ => None,
            })
            .collect()
    }

    /// A fresh console log path for a new start of `name`.
    fn new_log_path(&self, name: &str) -> PathBuf {
        let n = self.next_log.fetch_add(1, Ordering::SeqCst);
        self.logs_dir.join(format!("{name}.{n}.log"))
    }

    async fn run_vm(
        self: Arc<Self>,
        run: RunRequest,
        cred: UCred,
    ) -> Result<ApiResponse, ApiError> {
        validate_name(&run.name)?;
        if run.command.is_empty() {
            return Err(bad_request("no command given to run in the VM"));
        }
        if !run.rootfs.is_absolute() {
            return Err(bad_request("the rootfs path must be absolute"));
        }
        in_bounds("vcpus", run.vcpus, MIN_VCPUS, MAX_VCPUS)?;
        in_bounds("mem_mib", run.mem_mib, MIN_MEM_MIB, MAX_MEM_MIB)?;
        // Opened as the caller's own uid/gid, before anything else is
        // reserved: the agent runs as root, but a `cirro` group member
        // must never make it read a file (or device) they couldn't (#14).
        let rootfs_path = run.rootfs.clone();
        let (uid, gid) = (cred.uid(), cred.gid());
        let rootfs_file =
            tokio::task::spawn_blocking(move || rootfs_open::open_as(uid, gid, &rootfs_path))
                .await
                .map_err(|e| internal(format!("the rootfs-open helper panicked: {e}")))?
                .map_err(|e| match e {
                    rootfs_open::Error::Denied(msg) => ApiError(StatusCode::FORBIDDEN, msg),
                    rootfs_open::Error::Failed(msg) => internal(msg),
                })?;
        let log = self.new_log_path(&run.name);
        let (vm_address, replaced) = {
            let mut vms = self.vms.lock().unwrap();
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the Node agent is shutting down".into(),
                ));
            }
            if matches!(
                vms.get(&run.name),
                Some(Entry::Starting { .. } | Entry::Running { .. })
            ) {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("a VM named {:?} already exists", run.name),
                ));
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
            // Reusing an Ended VM's name replaces its record once the new VM
            // has started.
            let replaced = match vms.insert(
                run.name.clone(),
                Entry::Starting {
                    vm_address,
                    log: log.clone(),
                },
            ) {
                Some(Entry::Ended { info, log }) => Some(Replaced { info, log }),
                _ => None,
            };
            (vm_address, replaced)
        };

        let spec = VmSpec {
            vm_address,
            rootfs: run.rootfs,
            rootfs_file,
            mem_mib: run.mem_mib,
            vcpus: run.vcpus,
            command: run.command,
            console_log: log,
        };
        // Started on its own task, so the start runs to completion (success,
        // or a full unwind) even if the client disconnects and hyper drops
        // this request's future.
        let name = run.name;
        tokio::spawn(async move { self.start_vm(name, spec, replaced).await })
            .await
            .unwrap_or_else(|e| Err(internal(format!("starting the VM panicked: {e}"))))
    }

    /// Runs `Vm::start` for a name and VM address already reserved as
    /// `Entry::Starting`, then hands a started VM to its supervisor task.
    /// A VM that fails to start leaves no record and no log of its own, and
    /// puts back the Ended VM it would have replaced.
    async fn start_vm(
        self: Arc<Self>,
        name: String,
        spec: VmSpec,
        replaced: Option<Replaced>,
    ) -> Result<ApiResponse, ApiError> {
        let vm = match Vm::start(&self.node, &spec).await {
            Ok(vm) => vm,
            Err(e) => {
                self.forget_start(&name, replaced, &spec.console_log);
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
        // A VM that isn't on record would be nobody's to stop after a restart.
        let recorded =
            self.store
                .lock()
                .unwrap()
                .insert_running(&info, &spec.console_log, vm.process());
        if let Err(e) = recorded {
            vm.destroy().await;
            self.forget_start(&name, replaced, &spec.console_log);
            return Err(internal(format!(
                "VM {name:?} started, but recording it failed, so it was stopped: {e}"
            )));
        }
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(old.log);
        }
        self.start_supervising(info.clone(), spec.console_log, vm);
        Ok(json(StatusCode::CREATED, &info))
    }

    /// Records `vm` as running and spawns its supervisor task, which records
    /// it as ended once its host state is gone.
    fn start_supervising(self: &Arc<Self>, info: VmInfo, log: PathBuf, vm: Vm) {
        let name = info.name.clone();
        let (stops_tx, stops_rx) = mpsc::channel(4);
        let (ended_tx, ended_rx) = watch::channel(false);
        self.vms.lock().unwrap().insert(
            name.clone(),
            Entry::Running {
                info,
                log,
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
    }

    /// Brings the Node back in line with the state database after the agent
    /// was down. A VM recorded as running whose process is still there is
    /// taken back. One whose process is gone becomes an Ended VM. Then all
    /// host state in the Node subnet that no running VM holds is removed:
    /// what the dead VMs left, and what a start that never got recorded left,
    /// including its console log.
    ///
    /// Runs before the socket opens, so no request sees a half-restored Node.
    async fn reconcile(self: &Arc<Self>) -> io::Result<()> {
        let records = self.store.lock().unwrap().load()?;
        self.remove_unrecorded_logs(&records)?;
        for Record { info, log, process } in records {
            let name = info.name.clone();
            let entry = if info.ended.is_some() {
                Entry::Ended { info, log }
            } else {
                // A running VM's record always names its VM address and
                // process. One that doesn't can't be told apart from a live
                // VM, and treating it as dead would have the sweep kill it.
                let (Some(vm_address), Some(process)) = (info.vm_address, process) else {
                    return Err(io::Error::other(format!(
                        "the state database records VM {name:?} as running but doesn't name \
                         its VM address and process; refusing to start rather than guess"
                    )));
                };
                if process.is_running() {
                    let vm = Vm::adopt(&self.node, vm_address, process, log.clone());
                    self.start_supervising(info, log, vm);
                    continue;
                }
                let ended = Ended {
                    at: now(),
                    reason: EndReason::AgentDown,
                };
                self.store.lock().unwrap().mark_ended(&name, &ended)?;
                Entry::Ended {
                    info: ended_info(info, ended),
                    log,
                }
            };
            self.vms.lock().unwrap().insert(name, entry);
        }
        self.sweep_host_state().await;
        Ok(())
    }

    /// Removes the console logs no record names.
    fn remove_unrecorded_logs(&self, records: &[Record]) -> io::Result<()> {
        // By file name alone, so a state dir reached by another path can never
        // make every log look unrecorded.
        let recorded: Vec<_> = records.iter().filter_map(|r| r.log.file_name()).collect();
        for entry in std::fs::read_dir(&self.logs_dir)?.flatten() {
            if !recorded.contains(&entry.file_name().as_os_str()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Removes the host state in the Node subnet that no VM in `vms` holds.
    async fn sweep_host_state(&self) {
        let held: Vec<Ipv4Addr> = self
            .vms
            .lock()
            .unwrap()
            .values()
            .filter_map(Entry::vm_address)
            .collect();
        for owner in vm::host_state_owners(&self.node) {
            if let Some(vm_address) = self.subnet.vm_address_with_low16(owner)
                && !held.contains(&vm_address)
            {
                vm::clean_up(&self.node, vm_address).await;
            }
        }
    }

    /// Undoes the reservation of a start that failed: puts back the Ended VM
    /// it would have replaced, and removes the log of its own.
    fn forget_start(&self, name: &str, replaced: Option<Replaced>, log: &Path) {
        {
            let mut vms = self.vms.lock().unwrap();
            vms.remove(name);
            if let Some(Replaced { info, log }) = replaced {
                vms.insert(name.to_string(), Entry::Ended { info, log });
            }
        }
        let _ = std::fs::remove_file(log);
    }

    /// Turns a running VM's record into an Ended VM's, once its host state
    /// is gone.
    fn record_end(&self, name: &str, reason: EndReason) {
        let mut vms = self.vms.lock().unwrap();
        if let Some(Entry::Running { info, log, .. }) = vms.remove(name) {
            let ended = Ended { at: now(), reason };
            if let Err(e) = self.store.lock().unwrap().mark_ended(name, &ended) {
                eprintln!("cirro node: record the end of {name:?}: {e}");
            }
            let info = ended_info(info, ended);
            vms.insert(name.to_string(), Entry::Ended { info, log });
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
            Some(Entry::Ended { info, .. }) => Ok(json(StatusCode::OK, info)),
            _ => Err(internal(format!(
                "VM {name:?} ended, but its record is gone"
            ))),
        }
    }

    fn logs(&self, name: &str, offset: u64) -> Result<ApiResponse, ApiError> {
        let (state, log) = match self.vms.lock().unwrap().get(name) {
            None => return Err(no_such_vm(name)),
            Some(entry @ Entry::Ended { .. }) => ("ended", entry.log().clone()),
            Some(entry) => ("running", entry.log().clone()),
        };
        let mut bytes = Vec::new();
        if let Ok(mut file) = std::fs::File::open(log) {
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_to_end(&mut bytes))
                .map_err(|e| internal(format!("read the console log: {e}")))?;
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
            Some(Entry::Ended { log, .. }) => {
                self.store
                    .lock()
                    .unwrap()
                    .delete(name)
                    .map_err(|e| internal(format!("remove the record of {name:?}: {e}")))?;
                let _ = std::fs::remove_file(log);
                vms.remove(name);
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

    /// Waits for the VMs still starting to finish, so none is left without
    /// a record. Running VMs are left running: they outlive the agent.
    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        while self
            .vms
            .lock()
            .unwrap()
            .values()
            .any(|e| matches!(e, Entry::Starting { .. }))
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Only succeed once empty, so a jail still in use is never removed.
        let _ = std::fs::remove_dir(self.node.jail_base.join("firecracker"));
        let _ = std::fs::remove_dir(&self.node.jail_base);
        let _ = std::fs::remove_dir(vm::parent_cgroup());
    }
}

/// `info` as an Ended VM's: it holds no VM address any more.
fn ended_info(info: VmInfo, ended: Ended) -> VmInfo {
    VmInfo {
        vm_address: None,
        ended: Some(ended),
        ..info
    }
}

/// The number for the next console log: one past the highest that any log
/// file in `logs_dir` carries, so no start reuses a log of an earlier agent's.
fn next_log_number(logs_dir: &Path) -> u64 {
    std::fs::read_dir(logs_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            let (_, number) = name.strip_suffix(".log")?.rsplit_once('.')?;
            number.parse::<u64>().ok()
        })
        .max()
        .map_or(0, |highest| highest + 1)
}

/// `vcpus`/`mem_mib` (whichever `field` names) are within `[min, max]`, the
/// same shape checked for both.
fn in_bounds<T: PartialOrd + std::fmt::Display>(
    field: &str,
    value: T,
    min: T,
    max: T,
) -> Result<(), ApiError> {
    if value >= min && value <= max {
        Ok(())
    } else {
        Err(bad_request(&format!(
            "{field} must be between {min} and {max}, got {value}"
        )))
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

fn internal(message: String) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, message)
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
