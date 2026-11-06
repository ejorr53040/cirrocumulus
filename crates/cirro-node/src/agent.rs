//! The Node agent (ADR 0002): the long-running, privileged `cirro node
//! agent` process that owns every VM on this Node. It serves the
//! `cirro-proto` HTTP+JSON API on a Unix socket, allocates VM addresses and
//! drives `crate::vm` for each request.
//!
//! Each running VM is owned by its own supervisor task (`Vm::supervise`).
//! When the VM ends, for whatever reason, the task records it as an Ended VM
//! whose console log stays in the state dir until `rm` or name reuse.
//!
//! VMs outlive the agent (ADR 0002). Each VM is recorded in the state
//! database once it has started; the agent never stops VMs when it exits,
//! and a starting agent takes back every VM whose record still matches a
//! running process before it opens its socket.
//!
//! A park ends a VM with [`EndReason::Parked`] and keeps its snapshot in the
//! state dir under its name; a wake starts a new VM from that snapshot under
//! the same name, appending to the same console log.

use crate::egress;
use crate::layout;
use crate::metrics::{Recorder, RunningVm, Sampler};
use crate::rootfs_open;
use crate::state::{Record, Store};
use crate::subnet::Subnet;
use crate::vm::{self, Stop, Vm, VmSpec, WakeSpec};
use bytes::Bytes;
use cirro_edge::acme::Acme;
pub use cirro_edge::acme::AcmeConfig;
use cirro_edge::tls::NodeCa;
use cirro_edge::{Lease, Resolution, Router};
use cirro_guest_init::config::{self as guest, Config as GuestConfig};
use cirro_proto::{
    AddMachine, DEFAULT_STOP_TIMEOUT_SECS, EndReason, Ended, ErrorBody, LOG_CHUNK, MAX_MEM_MIB,
    MAX_VCPUS, MIN_MEM_MIB, MIN_VCPUS, MachineAppInfo, MachineAppRequest, MachineHostKey,
    MachineScan, Route, RunRequest, Stats, StopRequest, VM_STATE_HEADER, VmInfo, VmStats,
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
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::unix::UCred;
use tokio::net::{TcpListener, UnixListener};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;
use tracing::{Instrument, error, info, info_span, warn};

mod api;
mod edge;
mod machine;

use api::{ApiError, ApiResponse, bad_request, internal};
use edge::Edge;

/// How long the edge holds a request for an App that is starting or waking
/// before answering 503.
const HOLD_FOR_START: Duration = Duration::from_secs(30);

/// How often the agent looks for ACME certificates to get, renew, or try
/// again after a failure (which backs off on its own).
const RENEW_CHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// How often the agent looks for Apps that have been idle long enough to
/// park.
const IDLE_CHECK_EVERY: Duration = Duration::from_secs(1);

/// How often the agent samples the Node and every running VM.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

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
    /// Where the HTTP edge listens, if anywhere.
    pub http: Option<SocketAddr>,
    /// Where the HTTPS edge listens, if anywhere. Its certificates come
    /// from a CA of the Node's own, kept in the state dir.
    pub https: Option<SocketAddr>,
    /// With `https`: get public hostnames' certificates from this ACME CA
    /// instead, proved through the HTTP edge.
    pub acme: Option<AcmeConfig>,
}

/// One name's record in the registry. Every VM start gets its own console
/// log file (`log`), so a failed reuse of a name can't clobber the log of
/// the Ended VM it would have replaced.
enum Entry {
    /// `Vm::start` is running; the entry reserves the name and VM address.
    Starting {
        vm_address: Ipv4Addr,
        log: PathBuf,
        route: Option<Route>,
    },
    /// Owned by a supervisor task, which `stops` reaches. `ended` turns true
    /// once the task has recorded the VM as ended.
    Running {
        info: VmInfo,
        log: PathBuf,
        stops: mpsc::Sender<Stop>,
        ended: watch::Receiver<bool>,
        traffic: Arc<Traffic>,
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

    fn route(&self) -> Option<&Route> {
        match self {
            Entry::Starting { route, .. } => route.as_ref(),
            Entry::Running { info, .. } | Entry::Ended { info, .. } => info.route.as_ref(),
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

/// The edge's requests to one running VM, for idle park.
struct Traffic {
    /// Requests the edge is proxying to it now.
    in_flight: AtomicU32,
    /// When the last request finished, or the VM started if none has.
    last: Mutex<Instant>,
    /// Set while it is being parked: requests for it are held until the
    /// park ends, then wake it.
    parking: AtomicBool,
}

impl Traffic {
    fn new() -> Traffic {
        Traffic {
            in_flight: AtomicU32::new(0),
            last: Mutex::new(Instant::now()),
            parking: AtomicBool::new(false),
        }
    }

    /// Whether nothing has come or been in flight for `idle`.
    fn idle_for(&self, idle: Duration) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0 && self.last.lock().unwrap().elapsed() >= idle
    }
}

/// The right to park one VM: while it is held no other park starts, and
/// the edge holds requests for the VM. Released when dropped, however the
/// park ended.
struct ParkClaim(Arc<Traffic>);

impl ParkClaim {
    fn take(traffic: &Arc<Traffic>) -> Option<ParkClaim> {
        traffic
            .parking
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| ParkClaim(traffic.clone()))
    }
}

impl Drop for ParkClaim {
    fn drop(&mut self) {
        self.0.parking.store(false, Ordering::SeqCst);
    }
}

/// One request in flight to a VM, counted until dropped.
struct InFlight(Arc<Traffic>);

impl InFlight {
    fn start(traffic: &Arc<Traffic>) -> InFlight {
        traffic.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(traffic.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        *self.0.last.lock().unwrap() = Instant::now();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
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
    /// Console logs, one per start, kept with the Ended VM record.
    logs_dir: PathBuf,
    parked_dir: PathBuf,
    vms: Mutex<BTreeMap<String, Entry>>,
    store: Mutex<Store>,
    /// Numbers each VM start's console log file.
    next_log: AtomicU64,
    /// Set once shutdown begins, so no new VM starts while the agent waits for
    /// the starts already under way to finish.
    shutting_down: AtomicBool,
    /// What the Node and each running VM used, for `GET /stats`.
    metrics: Mutex<Recorder>,
    /// Told whenever a start or wake has finished, either way, so requests
    /// held for an App that was starting can look again.
    settled: Notify,
    /// Why each App's last wake failed, for requests that were held for it.
    wake_failures: Mutex<BTreeMap<String, String>>,
    /// The CA the HTTPS edge's certificates come from, when there is one.
    ca: Option<Arc<NodeCa>>,
    /// Where public hostnames' certificates come from, when ACME is on.
    acme: Option<Arc<Acme>>,
    machines: machine::Machines,
}

/// Tells requests held for a start, wake or park that it has finished,
/// however it finished, panics included.
struct Settled(Arc<Agent>);

impl Drop for Settled {
    fn drop(&mut self) {
        self.0.settled.notify_waiters();
    }
}

/// Runs the agent until SIGTERM or SIGINT, then removes the socket. VMs,
/// their records, console logs and snapshots stay, and the jail tree, the
/// parked dir and the parent cgroup go only if nothing is left in them. The
/// Node's egress policy is ensured first, and outlives the agent.
pub async fn run(config: Config) -> io::Result<()> {
    // Both refusals below run before the agent changes anything on the
    // Node: a live socket is checked first (the fastest way to tell this
    // start is a mistake), then the state dir's subnet.
    refuse_if_already_running(&config.socket)?;

    let logs_dir = layout::logs(&config.state_dir);
    std::fs::create_dir_all(&logs_dir)?;
    let parked_dir = layout::parked(&config.state_dir);
    match std::fs::DirBuilder::new().mode(0o700).create(&parked_dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    // Also when an earlier agent made it: it holds guests' memory.
    std::fs::set_permissions(&parked_dir, std::fs::Permissions::from_mode(0o700))?;
    let store = Store::open(
        &layout::state_db(&config.state_dir),
        &config.subnet.to_string(),
    )?;

    // Fail closed: no VM starts without the egress policy in place.
    let egress_iface = egress::default_route_iface();
    if egress_iface.is_none() {
        warn!("no IPv4 default route, so VMs won't reach the internet");
    }
    egress::enable_ip_forward(&layout::ip_forward_record(&config.state_dir))?;
    egress::ensure_node_policy(&config.subnet.to_string(), egress_iface.as_deref())
        .map_err(|e| io::Error::other(format!("apply the Node's egress policy: {e}")))?;
    let gid = resolve_group(&config.socket_group)?;
    let ca = match config.https {
        Some(_) => Some(Arc::new(NodeCa::load_or_create(&config.state_dir)?)),
        None => None,
    };
    let acme = match config.acme {
        Some(acme) if config.https.is_some() => Some(Acme::load(&config.state_dir, acme)?),
        Some(_) => {
            return Err(io::Error::other(
                "ACME certificates are for the HTTPS edge: start the agent with --https too",
            ));
        }
        None => None,
    };
    let agent = Arc::new(Agent {
        node: vm::NodeConfig {
            firecracker: config.firecracker,
            jailer: config.jailer,
            kernel: config.kernel,
            jail_base: layout::jail_base(&config.state_dir),
            node_address: config.subnet.node_address(),
            cirro_gid: gid,
        },
        subnet: config.subnet,
        logs_dir: logs_dir.clone(),
        parked_dir,
        vms: Mutex::new(BTreeMap::new()),
        store: Mutex::new(store),
        next_log: AtomicU64::new(next_log_number(&logs_dir)),
        shutting_down: AtomicBool::new(false),
        metrics: Mutex::new(Recorder::default()),
        settled: Notify::new(),
        wake_failures: Mutex::new(BTreeMap::new()),
        ca,
        acme,
        machines: machine::Machines::new(&config.state_dir),
    });
    agent.reconcile().await?;
    agent.machines.restore(&agent.store)?;
    let idle_parker = tokio::spawn(agent.clone().park_idle_apps());
    let renewer = agent
        .acme
        .is_some()
        .then(|| tokio::spawn(agent.clone().renew_certificates()));
    // Bound before the socket opens, so an edge that can't listen stops the
    // start instead of leaving a Node whose Apps nothing can reach.
    let edge = match config.http {
        Some(address) => {
            let listener = TcpListener::bind(address)
                .await
                .map_err(|e| io::Error::other(format!("listen for HTTP on {address}: {e}")))?;
            info!(%address, "HTTP edge listening");
            Some(tokio::spawn(cirro_edge::serve(
                listener,
                Arc::new(Edge(agent.clone())),
                agent.acme.clone(),
            )))
        }
        None => None,
    };
    // `agent.ca` is set exactly when `config.https` is.
    let tls_edge = match (config.https, &agent.ca) {
        (Some(address), Some(ca)) => {
            let listener = TcpListener::bind(address)
                .await
                .map_err(|e| io::Error::other(format!("listen for HTTPS on {address}: {e}")))?;
            let serving = cirro_edge::serve_tls(
                listener,
                Arc::new(Edge(agent.clone())),
                ca.clone(),
                agent.acme.clone(),
            )?;
            info!(%address, "HTTPS edge listening");
            Some(tokio::spawn(serving))
        }
        _ => None,
    };
    let sampling = agent.clone();
    let sampler = tokio::spawn(async move {
        let sampler = Sampler::new(PathBuf::from("/"));
        let mut every_second = tokio::time::interval(SAMPLE_EVERY);
        // After a stall, wait for the next whole second rather than catch
        // up with samples milliseconds apart, whose rates would be noise.
        every_second.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            every_second.tick().await;
            sampling.sample(&sampler);
        }
    });

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

    info!(socket = %config.socket.display(), subnet = %config.subnet, "Node agent ready");
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!("accept: {e}");
                        continue;
                    }
                };
                let cred = match stream.peer_cred() {
                    Ok(cred) => cred,
                    Err(e) => {
                        warn!("read peer credentials: {e}");
                        continue;
                    }
                };
                let agent = agent.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let agent = agent.clone();
                        let span = info_span!(
                            "request",
                            method = %req.method(),
                            path = req.uri().path(),
                            uid = cred.uid(),
                        );
                        async move { Ok::<_, hyper::Error>(agent.handle(req, cred).await) }
                            .instrument(span)
                    });
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        warn!("connection: {e}");
                    }
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    info!("shutting down; running VMs stay up");
    sampler.abort();
    idle_parker.abort();
    if let Some(renewer) = renewer {
        renewer.abort();
    }
    for edge in [edge, tls_edge].into_iter().flatten() {
        edge.abort();
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

impl Agent {
    /// Samples the Node and every running VM, for [`Agent::stats`].
    fn sample(&self, sampler: &Sampler) {
        let running: Vec<RunningVm> = self
            .list(false)
            .into_iter()
            .filter_map(|info| {
                Some(RunningVm {
                    id: vm::host_id(info.vm_address?),
                    name: info.name,
                })
            })
            .collect();
        // The kernel's files are read under neither the VM table's lock
        // nor the metrics one.
        let at = Instant::now();
        let readings = sampler.read(&running);
        self.metrics.lock().unwrap().record(at, readings);
    }

    fn stats(&self) -> Stats {
        let running = self.list(false);
        let metrics = self.metrics.lock().unwrap();
        Stats {
            now: now(),
            node: metrics.node(),
            vms: running
                .into_iter()
                .map(|info| VmStats {
                    history: metrics.vm(&info.name),
                    info,
                })
                .collect(),
        }
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

    async fn run_vm(self: Arc<Self>, run: RunRequest, cred: UCred) -> Result<VmInfo, ApiError> {
        validate_name("VM", &run.name)?;
        let guest = guest_config(&run)?;
        if !run.rootfs.is_absolute() {
            return Err(bad_request("the rootfs path must be absolute"));
        }
        in_bounds("vcpus", run.vcpus, MIN_VCPUS, MAX_VCPUS)?;
        in_bounds("mem_mib", run.mem_mib, MIN_MEM_MIB, MAX_MEM_MIB)?;
        if let Some(route) = &run.route {
            validate_route(route)?;
        }
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
            self.refuse_if_shutting_down()?;
            if self.machines.has_app(&run.name) {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("a Machine-backed App named {:?} already exists", run.name),
                ));
            }
            match vms.get(&run.name) {
                Some(Entry::Starting { .. } | Entry::Running { .. }) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("a VM named {:?} already exists", run.name),
                    ));
                }
                Some(Entry::Ended { info, .. }) if is_parked(info) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!(
                            "VM {:?} is parked: wake it, or rm it to discard its snapshot",
                            run.name
                        ),
                    ));
                }
                _ => {}
            }
            if let Some(route) = &run.route
                && let Some(owner) = self.host_owner(&vms, &route.host)
                && owner.name() != run.name
            {
                return Err(owner.taken(&route.host));
            }
            // Reusing an Ended VM's name replaces its record once the new VM
            // has started.
            let (vm_address, replaced) =
                self.reserve(&mut vms, &run.name, log.clone(), run.route.clone())?;
            let replaced = match replaced {
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
            guest,
            console_log: log,
        };
        let (name, route) = (run.name, run.route);
        let agent = self.clone();
        self.detached("starting", agent.start_vm(name, spec, route, replaced))
            .await
    }

    /// Runs `work` on its own task, so it runs to completion (success, or a
    /// full unwind) even if the client disconnects and hyper drops the
    /// request's future. Held requests are told to look again once it ends,
    /// after everything `work` holds is released.
    async fn detached(
        self: &Arc<Self>,
        what: &str,
        work: impl Future<Output = Result<VmInfo, ApiError>> + Send + 'static,
    ) -> Result<VmInfo, ApiError> {
        let settled = Settled(self.clone());
        tokio::spawn(
            async move {
                let _settled = settled;
                work.await
            }
            .in_current_span(),
        )
        .await
        .unwrap_or_else(|e| Err(internal(format!("{what} the VM panicked: {e}"))))
    }

    fn refuse_if_shutting_down(&self) -> Result<(), ApiError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the Node agent is shutting down".into(),
            ));
        }
        Ok(())
    }

    /// Reserves `name` and the lowest VM address no entry in `vms` holds as
    /// [`Entry::Starting`], for a start or a wake, and gives back the entry
    /// it replaced.
    fn reserve(
        &self,
        vms: &mut BTreeMap<String, Entry>,
        name: &str,
        log: PathBuf,
        route: Option<Route>,
    ) -> Result<(Ipv4Addr, Option<Entry>), ApiError> {
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
        let starting = Entry::Starting {
            vm_address,
            log,
            route,
        };
        Ok((vm_address, vms.insert(name.to_string(), starting)))
    }

    /// Snapshots a running VM and ends it as parked. A VM that can't be
    /// parked keeps running, and the error says why.
    async fn park_vm(self: Arc<Self>, name: String) -> Result<VmInfo, ApiError> {
        let claim = self.claim_park(&name)?;
        self.park_claimed(name, claim).await
    }

    /// Claims the park of a running VM, or says why it can't be parked.
    fn claim_park(&self, name: &str) -> Result<ParkClaim, ApiError> {
        if let Some(Entry::Running { traffic, .. }) = self.vms.lock().unwrap().get(name) {
            return ParkClaim::take(traffic).ok_or_else(|| {
                ApiError(
                    StatusCode::CONFLICT,
                    format!("VM {name:?} is already being parked"),
                )
            });
        }
        Err(self.running(name).err().unwrap_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                format!("VM {name:?} started while it was being parked; try again"),
            )
        }))
    }

    /// Parks a VM whose park `claim` holds. Detached, so the park finishes,
    /// and the claim is released, even if the client goes away.
    async fn park_claimed(
        self: Arc<Self>,
        name: String,
        claim: ParkClaim,
    ) -> Result<VmInfo, ApiError> {
        let agent = self.clone();
        self.detached("parking", async move {
            let _claim = claim;
            agent.park(&name).await
        })
        .await
    }

    async fn park(&self, name: &str) -> Result<VmInfo, ApiError> {
        let (stops, mut ended) = self.running(name)?;
        let (done_tx, done) = oneshot::channel();
        let park = Stop::Park {
            into: self.parked_dir.join(name),
            done: done_tx,
        };
        if stops.send(park).await.is_err() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("VM {name:?} ended before it could be parked"),
            ));
        }
        match done.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("VM {name:?} couldn't be parked: {e}"),
                ));
            }
            Err(_) => {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("VM {name:?} ended before it could be parked"),
                ));
            }
        }
        let _ = ended.wait_for(|ended| *ended).await;
        self.ended_info(name)
    }

    /// Starts a new VM from a parked VM's snapshot, under its name. A wake
    /// that fails leaves the VM parked.
    async fn wake_vm(self: Arc<Self>, name: String) -> Result<VmInfo, ApiError> {
        let (spec, parked) = {
            let mut vms = self.vms.lock().unwrap();
            self.refuse_if_shutting_down()?;
            let parked = match vms.get(&name) {
                None => return Err(no_such_vm(&name)),
                Some(Entry::Ended { info, .. })
                    if is_parked(info) && !self.parked_dir.join(&name).is_dir() =>
                {
                    return Err(ApiError(StatusCode::CONFLICT, snapshot_gone(&name)));
                }
                Some(Entry::Ended { info, log }) if is_parked(info) => Replaced {
                    info: info.clone(),
                    log: log.clone(),
                },
                Some(Entry::Ended { .. }) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!(
                            "VM {name:?} isn't parked; it has ended, and `cirro run` starts \
                             a new VM under its name"
                        ),
                    ));
                }
                Some(_) => {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("VM {name:?} isn't parked; it's running"),
                    ));
                }
            };
            let (vm_address, _) = self.reserve(
                &mut vms,
                &name,
                parked.log.clone(),
                parked.info.route.clone(),
            )?;
            let spec = WakeSpec {
                vm_address,
                mem_mib: parked.info.mem_mib,
                vcpus: parked.info.vcpus,
                snapshot: self.parked_dir.join(&name),
                console_log: parked.log.clone(),
            };
            (spec, parked)
        };
        let agent = self.clone();
        self.detached("waking", async move {
            let woken = agent.clone().finish_wake(name.clone(), spec, parked).await;
            let mut failures = agent.wake_failures.lock().unwrap();
            match &woken {
                Ok(_) => failures.remove(&name),
                Err(ApiError(_, why)) => failures.insert(name, why.clone()),
            };
            woken
        })
        .await
    }

    async fn finish_wake(
        self: Arc<Self>,
        name: String,
        spec: WakeSpec,
        parked: Replaced,
    ) -> Result<VmInfo, ApiError> {
        let put_back = |agent: &Agent, parked: Replaced| {
            agent.vms.lock().unwrap().insert(
                name.clone(),
                Entry::Ended {
                    info: parked.info,
                    log: parked.log,
                },
            );
        };
        let vm = match Vm::wake(&self.node, &spec)
            .instrument(info_span!("wake", vm = name, address = %spec.vm_address))
            .await
        {
            Ok(vm) => vm,
            Err(e) => {
                warn!(vm = name, "failed to wake: {e}");
                put_back(&self, parked);
                return Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("VM {name:?} failed to wake, and is still parked: {e}"),
                ));
            }
        };
        let started = started_info(
            &name,
            spec.vm_address,
            spec.mem_mib,
            spec.vcpus,
            parked.info.route.clone(),
        );
        // Emptied by the wake, and gone before a park of the new VM can make
        // it again.
        let _ = std::fs::remove_dir(&spec.snapshot);
        let info = match self.keep(started, &spec.console_log, vm).await {
            Ok(info) => info,
            Err(e) => {
                // The guest has run, so its rootfs may no longer match the
                // snapshot, which goes with the VM. It ends as forced, not
                // parked: there is nothing left to wake.
                let ended = Ended {
                    at: now(),
                    reason: EndReason::Forced,
                };
                if let Err(e) = self.store.lock().unwrap().mark_ended(&name, &ended) {
                    error!(vm = name, "record the end: {e}");
                }
                self.vms.lock().unwrap().insert(
                    name.clone(),
                    Entry::Ended {
                        info: ended_info(parked.info, ended),
                        log: parked.log,
                    },
                );
                return Err(internal(format!(
                    "VM {name:?} woke, but recording it failed, so it was stopped and its \
                     snapshot is lost: {e}"
                )));
            }
        };
        info!(vm = name, address = %spec.vm_address, "VM woke");
        Ok(info)
    }

    /// Runs `Vm::start` for a name and VM address already reserved as
    /// `Entry::Starting`, then hands a started VM to its supervisor task.
    /// A VM that fails to start leaves no record and no log of its own, and
    /// puts back the Ended VM it would have replaced.
    async fn start_vm(
        self: Arc<Self>,
        name: String,
        spec: VmSpec,
        route: Option<Route>,
        replaced: Option<Replaced>,
    ) -> Result<VmInfo, ApiError> {
        let vm = match Vm::start(&self.node, &spec)
            .instrument(info_span!("start", vm = name, address = %spec.vm_address))
            .await
        {
            Ok(vm) => vm,
            Err(e) => {
                warn!(vm = name, "failed to start: {e}");
                self.forget_start(&name, replaced, &spec.console_log);
                return Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("VM {name:?} failed to start: {e}"),
                ));
            }
        };
        let started = started_info(&name, spec.vm_address, spec.mem_mib, spec.vcpus, route);
        let info = match self.keep(started, &spec.console_log, vm).await {
            Ok(info) => info,
            Err(e) => {
                self.forget_start(&name, replaced, &spec.console_log);
                return Err(internal(format!(
                    "VM {name:?} started, but recording it failed, so it was stopped: {e}"
                )));
            }
        };
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(old.log);
        }
        info!(vm = name, address = %spec.vm_address, "VM started");
        if let Some(route) = &info.route {
            self.get_certificate(&route.host);
        }
        Ok(info)
    }

    /// Records a VM that just started or woke in the state database and
    /// hands it to its supervisor. A VM that isn't on record would be
    /// nobody's to stop after a restart, so one that can't be recorded is
    /// destroyed.
    async fn keep(self: &Arc<Self>, info: VmInfo, log: &Path, vm: Vm) -> Result<VmInfo, String> {
        let recorded = self
            .store
            .lock()
            .unwrap()
            .insert_running(&info, log, vm.process());
        if let Err(e) = recorded {
            vm.destroy().await;
            return Err(e.to_string());
        }
        self.start_supervising(info.clone(), log.to_path_buf(), vm);
        Ok(info)
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
                traffic: Arc::new(Traffic::new()),
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
        self.remove_unowned_snapshots(&records)?;
        for Record { info, log, process } in records {
            let name = info.name.clone();
            if is_parked(&info) && !self.parked_dir.join(&name).is_dir() {
                warn!(vm = name, "{}", snapshot_gone(&name));
            }
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
                    info!(vm = name, "re-adopted a running VM");
                    let vm = Vm::adopt(&self.node, vm_address, process, log.clone());
                    // Firecracker accepts this for a VM that already runs.
                    if let Err(e) = vm.resume().await {
                        warn!(vm = name, "{e}");
                    }
                    self.start_supervising(info, log, vm);
                    continue;
                }
                let ended = Ended {
                    at: now(),
                    reason: EndReason::AgentDown,
                };
                info!(vm = name, "VM ended while the agent was down");
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

    /// Removes whatever in the parked dir no parked VM's record owns: a
    /// snapshot left by a park the agent died part-way through, before the
    /// VM was recorded as parked, or one whose record is gone. Either holds a
    /// guest's memory that nothing could wake or remove.
    fn remove_unowned_snapshots(&self, records: &[Record]) -> io::Result<()> {
        let parked: Vec<&str> = records
            .iter()
            .filter(|r| is_parked(&r.info))
            .map(|r| r.info.name.as_str())
            .collect();
        for entry in std::fs::read_dir(&self.parked_dir)?.flatten() {
            if parked.iter().any(|name| entry.file_name() == **name) {
                continue;
            }
            let path = entry.path();
            warn!(path = %path.display(), "removing a snapshot no parked VM owns");
            let removed = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(e) = removed {
                warn!(path = %path.display(), "remove it: {e}");
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
        let jails = self.node.jails();
        for vm_address in vm::host_state_in(&jails, &self.subnet) {
            if !held.contains(&vm_address) {
                vm::clean_up(&jails, vm_address).await;
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
            info!(vm = name, reason = reason.as_str(), "VM ended");
            let ended = Ended { at: now(), reason };
            if let Err(e) = self.store.lock().unwrap().mark_ended(name, &ended) {
                error!(vm = name, "record the end: {e}");
            }
            let info = ended_info(info, ended);
            vms.insert(name.to_string(), Entry::Ended { info, log });
        }
    }

    /// The way to reach a running VM's supervisor, and to hear it end.
    fn running(&self, name: &str) -> Result<(mpsc::Sender<Stop>, watch::Receiver<bool>), ApiError> {
        let vms = self.vms.lock().unwrap();
        match vms.get(name) {
            None => Err(no_such_vm(name)),
            Some(Entry::Starting { .. }) => Err(ApiError(
                StatusCode::CONFLICT,
                format!("VM {name:?} is still starting"),
            )),
            Some(Entry::Ended { info, .. }) if is_parked(info) => Err(ApiError(
                StatusCode::CONFLICT,
                format!("VM {name:?} is parked: wake it first, or rm it to discard its snapshot"),
            )),
            Some(Entry::Ended { .. }) => Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "VM {name:?} has already ended; `cirro run` starts a new VM under its name"
                ),
            )),
            Some(Entry::Running { stops, ended, .. }) => Ok((stops.clone(), ended.clone())),
        }
    }

    /// Parks every App that has had no request for its idle time, once a
    /// second, until the task is aborted. An App whose park fails keeps
    /// running and gets a whole idle time before the next try.
    async fn park_idle_apps(self: Arc<Self>) {
        let mut every = tokio::time::interval(IDLE_CHECK_EVERY);
        every.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            every.tick().await;
            // Claimed under the VM table's lock, which every request takes
            // to reach a VM: none can slip in between the check and the park.
            let idle: Vec<(String, Arc<Traffic>, ParkClaim)> = self
                .vms
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(name, entry)| match entry {
                    Entry::Running { info, traffic, .. } => {
                        let secs = info.route.as_ref()?.idle_park_secs?;
                        if !traffic.idle_for(Duration::from_secs(secs.into())) {
                            return None;
                        }
                        let claim = ParkClaim::take(traffic)?;
                        Some((name.clone(), traffic.clone(), claim))
                    }
                    _ => None,
                })
                .collect();
            for (name, traffic, claim) in idle {
                let agent = self.clone();
                tokio::spawn(async move {
                    match agent.park_claimed(name.clone(), claim).await {
                        Ok(_) => info!(vm = name, "parked an idle App"),
                        Err(ApiError(_, why)) => {
                            warn!(vm = name, "couldn't park an idle App: {why}");
                            *traffic.last.lock().unwrap() = Instant::now();
                        }
                    }
                });
            }
        }
    }

    /// Starts getting `host` a certificate from the ACME CA, in the
    /// background, if ACME is on.
    fn get_certificate(&self, host: &str) {
        let Some(acme) = self.acme.clone() else {
            return;
        };
        let host = host.to_string();
        tokio::spawn(async move {
            if let Err(e) = acme.ensure(&host).await {
                warn!(host, "get a certificate from the ACME CA: {e}");
            }
        });
    }

    /// Gets every App's public hostname a certificate, and renews those
    /// near expiry, at start and then every [`RENEW_CHECK_EVERY`], until the
    /// task is aborted.
    async fn renew_certificates(self: Arc<Self>) {
        let mut every = tokio::time::interval(RENEW_CHECK_EVERY);
        loop {
            every.tick().await;
            let hosts: Vec<String> = self
                .vms
                .lock()
                .unwrap()
                .values()
                .filter_map(|entry| Some(entry.route()?.host.clone()))
                .collect();
            let machine_hosts = self.machines.apps().into_iter().map(|app| app.route.host);
            for host in hosts.into_iter().chain(machine_hosts) {
                self.get_certificate(&host);
            }
        }
    }

    /// The Ended VM record of `name`, once its supervisor has recorded it.
    fn ended_info(&self, name: &str) -> Result<VmInfo, ApiError> {
        match self.vms.lock().unwrap().get(name) {
            Some(Entry::Ended { info, .. }) => Ok(info.clone()),
            _ => Err(internal(format!(
                "VM {name:?} ended, but its record is gone"
            ))),
        }
    }

    async fn stop_vm(&self, name: &str, stop: StopRequest) -> Result<VmInfo, ApiError> {
        let (stops, mut ended) = self.running(name)?;
        let request = if stop.force {
            Stop::Force
        } else {
            Stop::Graceful {
                timeout: Duration::from_secs(
                    stop.timeout_secs.unwrap_or(DEFAULT_STOP_TIMEOUT_SECS),
                ),
            }
        };
        // If the supervisor is already gone, the VM is ending anyway.
        let _ = stops.send(request).await;
        let _ = ended.wait_for(|ended| *ended).await;
        self.ended_info(name)
    }

    fn logs(&self, name: &str, offset: u64) -> Result<ApiResponse, ApiError> {
        let (state, log) = match self.vms.lock().unwrap().get(name) {
            None => return Err(no_such_vm(name)),
            Some(entry @ Entry::Ended { .. }) => ("ended", entry.log().clone()),
            Some(entry) => ("running", entry.log().clone()),
        };
        let bytes =
            read_log(&log, offset).map_err(|e| internal(format!("read the console log: {e}")))?;
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; charset=utf-8")
            .header(VM_STATE_HEADER, state)
            .body(Full::new(Bytes::from(bytes)))
            .expect("build logs response"))
    }

    /// Makes a Machine-backed App, whose name and hostname mustn't be a
    /// VM's or a VM-backed App's.
    fn run_on_machine(&self, request: MachineAppRequest) -> Result<MachineAppInfo, ApiError> {
        validate_name("App", &request.name)?;
        validate_route(&request.route)?;
        if request.route.idle_park_secs.is_some() {
            return Err(bad_request(
                "a Machine-backed App can't idle-park: Cirrocumulus doesn't run what serves it",
            ));
        }
        // Held through the add, so no VM takes the name or hostname meanwhile.
        let vms = self.vms.lock().unwrap();
        if vms.contains_key(&request.name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "a VM named {:?} already exists; rm it to use its name",
                    request.name
                ),
            ));
        }
        if self.machines.has_app(&request.name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("an App named {:?} already exists", request.name),
            ));
        }
        if let Some(owner) = self.host_owner(&vms, &request.route.host) {
            return Err(owner.taken(&request.route.host));
        }
        let info = self.machines.add_app(request, &self.store)?;
        self.get_certificate(&info.route.host);
        Ok(info)
    }

    fn remove(&self, name: &str) -> Result<(), ApiError> {
        if self.machines.remove_app(name, &self.store)? {
            return Ok(());
        }
        let mut vms = self.vms.lock().unwrap();
        match vms.get(name) {
            None => Err(no_such_vm(name)),
            Some(Entry::Ended { info, log }) => {
                if is_parked(info) {
                    let snapshot = self.parked_dir.join(name);
                    match std::fs::remove_dir_all(&snapshot) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            return Err(internal(format!("remove the snapshot of {name:?}: {e}")));
                        }
                        _ => {}
                    }
                }
                self.store
                    .lock()
                    .unwrap()
                    .delete(name)
                    .map_err(|e| internal(format!("remove the record of {name:?}: {e}")))?;
                let _ = std::fs::remove_file(log);
                vms.remove(name);
                Ok(())
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
        self.machines.disconnect_all();
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            let starting = self
                .vms
                .lock()
                .unwrap()
                .values()
                .any(|e| matches!(e, Entry::Starting { .. }));
            if !starting {
                break;
            }
            settled.await;
        }
        // Only succeed once empty, so a jail still in use is never removed.
        let _ = std::fs::remove_dir(self.node.jails());
        let _ = std::fs::remove_dir(&self.node.jail_base);
        let _ = std::fs::remove_dir(&self.parked_dir);
        let _ = std::fs::remove_dir(vm::parent_cgroup());
    }
}

/// The App whose route has `host`, if any: its name and entry.
fn app_with_host<'a>(
    vms: &'a BTreeMap<String, Entry>,
    host: &str,
) -> Option<(&'a String, &'a Entry)> {
    vms.iter()
        .find(|(_, entry)| entry.route().is_some_and(|r| r.host == host))
}

/// The App a hostname belongs to. VMs and Machine-backed Apps share one
/// namespace of names and hostnames.
enum Owner {
    Vm(String),
    Machine(String),
}

impl Owner {
    fn name(&self) -> &str {
        match self {
            Owner::Vm(name) | Owner::Machine(name) => name,
        }
    }

    /// `host` is already this App's, which has to stop before `rm` when
    /// it's VM-backed.
    fn taken(&self, host: &str) -> ApiError {
        let (owner, stop) = match self {
            Owner::Vm(name) => (name, "stop and "),
            Owner::Machine(name) => (name, ""),
        };
        ApiError(
            StatusCode::CONFLICT,
            format!(
                "the hostname {host:?} belongs to App {owner:?}; {stop}rm it to free the hostname"
            ),
        )
    }
}

impl Agent {
    /// The App whose Route has `host`, of either kind. `vms` is the VM
    /// table, held by the caller so no VM takes the hostname meanwhile.
    fn host_owner(&self, vms: &BTreeMap<String, Entry>, host: &str) -> Option<Owner> {
        if let Some((name, _)) = app_with_host(vms, host) {
            return Some(Owner::Vm(name.clone()));
        }
        self.machines.app_with_host(host).map(Owner::Machine)
    }
}

/// A route's hostname is one the edge accepts ([`cirro_edge::is_hostname`]),
/// and its port isn't 0.
fn validate_route(route: &Route) -> Result<(), ApiError> {
    let host = &route.host;
    if !cirro_edge::is_hostname(host) {
        return Err(bad_request(&format!(
            "{host:?} isn't a hostname: use lowercase letters, digits, hyphens and dots, \
             like web.example.com"
        )));
    }
    if route.port == 0 {
        return Err(bad_request("the route's port must be 1-65535"));
    }
    Ok(())
}

fn is_parked(info: &VmInfo) -> bool {
    info.ended
        .as_ref()
        .is_some_and(|e| e.reason == EndReason::Parked)
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

/// Up to [`LOG_CHUNK`] bytes of a console log from `offset`: empty past its
/// end, or when it doesn't exist yet. A long-running App's log has no
/// bound, so it is never read whole.
fn read_log(log: &Path, offset: u64) -> io::Result<Vec<u8>> {
    let mut file = match std::fs::File::open(log) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(offset))?;
    file.take(LOG_CHUNK).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A VM that has just started (or woken) at `vm_address`.
fn started_info(
    name: &str,
    vm_address: Ipv4Addr,
    mem_mib: u32,
    vcpus: u8,
    route: Option<Route>,
) -> VmInfo {
    VmInfo {
        name: name.to_owned(),
        vm_address: Some(vm_address),
        mem_mib,
        vcpus,
        started_at: now(),
        ended: None,
        route,
    }
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

/// guest-init's config for `run`, refusing what guest-init, as PID 1, can't
/// use: it would crash the guest instead of failing the request.
fn guest_config(run: &RunRequest) -> Result<GuestConfig, ApiError> {
    let (exec, args) = run
        .command
        .split_first()
        .ok_or_else(|| bad_request("no command given to run in the VM"))?;
    if exec.is_empty() || run.command.iter().any(|a| a.contains('\0')) {
        return Err(bad_request(
            "the command must be non-empty and can't contain NUL bytes",
        ));
    }
    for entry in &run.env {
        let valid = entry
            .split_once('=')
            .is_some_and(|(key, _)| !key.is_empty())
            && !entry.contains('\0');
        if !valid {
            return Err(bad_request(&format!(
                "env entry {entry:?} must be KEY=VALUE with a non-empty KEY and no NUL bytes"
            )));
        }
    }
    if let Some(dir) = &run.workdir
        && (!dir.starts_with('/') || dir.contains('\0'))
    {
        return Err(bad_request(&format!(
            "workdir {dir:?} must be an absolute path with no NUL bytes"
        )));
    }
    Ok(GuestConfig {
        exec: exec.clone(),
        args: args.to_vec(),
        env: run.env.clone(),
        workdir: run.workdir.clone(),
        user: run.user.map(|u| guest::User {
            uid: u.uid,
            gid: u.gid,
        }),
    })
}

/// VM, Machine and App names are 1-32 characters of lowercase letters, digits and hyphens,
/// so they're safe in file names, URLs and log lines.
/// `kind` names what the name is for, in the error: VM, Machine or App.
fn validate_name(kind: &str, name: &str) -> Result<(), ApiError> {
    let valid = (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(bad_request(&format!(
            "invalid {kind} name {name:?}: a name is 1-32 characters of lowercase letters, \
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

fn snapshot_gone(name: &str) -> String {
    format!(
        "VM {name:?} is parked, but its snapshot is gone, so it can't be woken; \
         `cirro rm {name}` removes it"
    )
}

fn no_such_vm(name: &str) -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        format!("no VM named {name:?}; `cirro ps -a` lists them all"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_is_read_a_chunk_at_a_time_from_the_offset() {
        let dir = crate::test_util::tempdir("cirro-agent-logs");
        let log = dir.join("web.0.log");
        let mut contents = vec![b'a'; LOG_CHUNK as usize];
        contents.extend_from_slice(b"tail");
        std::fs::write(&log, &contents).unwrap();

        assert_eq!(read_log(&log, 0).unwrap().len(), LOG_CHUNK as usize);
        assert_eq!(read_log(&log, LOG_CHUNK).unwrap(), b"tail");
        assert!(read_log(&log, LOG_CHUNK + 4).unwrap().is_empty());
        assert!(read_log(&dir.join("missing.log"), 0).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
