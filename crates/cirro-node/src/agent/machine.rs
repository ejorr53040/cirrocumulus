//! Machines: computers the Node agent reaches over SSH, which don't run the
//! agent themselves (ADR 0010). The agent logs in with a key of its own and
//! trusts only the host key pinned when the Machine was added. It keeps one
//! OpenSSH `ssh` per Machine running to know whether the Machine is up.

use super::{ApiError, bad_request, internal};
use crate::state::{MachineAppRecord, MachineRecord, Store};
use cirro_edge::{Lease, Resolution};
use cirro_proto::{
    AddMachine, MachineAppInfo, MachineAppRequest, MachineInfo, MachineStatus, Route,
};
use hyper::StatusCode;
use nix::sys::signal::Signal;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tracing::warn;

pub(super) struct Machines {
    /// The agent's SSH private key, root-only (`ssh-keygen` makes it
    /// 0600); its public key is next to it with `.pub`.
    key: PathBuf,
    /// Every Machine's pinned host key, each under the Machine's name.
    known_hosts: PathBuf,
    /// Held while the agent's key is made, so two first uses make one key.
    keygen: Mutex<()>,
    /// Held for a whole add, so two adds can't take one name or pin each
    /// other's host key.
    adding: tokio::sync::Mutex<()>,
    entries: Mutex<BTreeMap<String, Machine>>,
}

struct Machine {
    record: MachineRecord,
    /// Its Machine-backed Apps, by name.
    apps: BTreeMap<String, MachineApp>,
    /// What the current `ssh` says: each new `ssh` gets a fresh one, so an
    /// old one ending can't mark the new one down.
    link: Arc<Mutex<Link>>,
    /// Owns the Machine's `ssh`, which is killed when the task is dropped.
    tunnel: JoinHandle<()>,
}

struct MachineApp {
    route: Route,
    /// The port on the Node's 127.0.0.1 that `ssh` forwards to the route's
    /// port on the Machine's localhost. Chosen afresh on each connect, so
    /// a new `ssh` never waits for the old one to let go of it.
    local_port: u16,
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.tunnel.abort();
    }
}

/// What the Machine's `ssh` last said about the connection.
struct Link {
    status: MachineStatus,
    last_error: Option<String>,
    /// The Machine presented a host key other than the pinned one, so no
    /// `ssh` runs to it again until it's removed and added.
    host_key_changed: bool,
}

impl Machines {
    pub(super) fn new(state_dir: &Path) -> Machines {
        Machines {
            key: state_dir.join("ssh_key"),
            known_hosts: state_dir.join("known_hosts"),
            keygen: Mutex::new(()),
            adding: tokio::sync::Mutex::new(()),
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Brings back the Machines a previous agent added, connecting to each
    /// in the background so an unreachable one holds nothing up.
    pub(super) fn restore(&self, store: &Mutex<Store>) -> io::Result<()> {
        let (records, app_records) = {
            let store = store.lock().unwrap();
            (store.load_machines()?, store.load_machine_apps()?)
        };
        for record in records {
            let apps = app_records
                .iter()
                .filter(|app| app.machine == record.name)
                .map(|app| {
                    let route = Route {
                        host: app.host.clone(),
                        port: app.port,
                        idle_park_secs: None,
                    };
                    (app.name.clone(), route)
                })
                .collect();
            // One Machine that can't be set up mustn't stop the agent starting.
            let name = record.name.clone();
            if let Err(e) = self.connect(record, apps) {
                warn!("restore Machine {name:?}: {e}");
            }
        }
        Ok(())
    }

    pub(super) fn apps(&self) -> Vec<MachineAppInfo> {
        let entries = self.entries.lock().unwrap();
        entries
            .values()
            .flat_map(|machine| {
                let status = machine.link.lock().unwrap().status;
                machine.apps.iter().map(move |(name, app)| MachineAppInfo {
                    name: name.clone(),
                    machine: machine.record.name.clone(),
                    route: app.route.clone(),
                    status,
                })
            })
            .collect()
    }

    pub(super) fn has_app(&self, name: &str) -> bool {
        has_app(&self.entries.lock().unwrap(), name)
    }

    /// The Machine-backed App whose route has `host`, if any.
    pub(super) fn app_with_host(&self, host: &str) -> Option<String> {
        app_with_host(&self.entries.lock().unwrap(), host)
    }

    /// Where the edge sends a request for `host`, when a Machine-backed App
    /// has it: through its Machine's `ssh` while that's up.
    pub(super) fn resolve(&self, host: &str) -> Option<Resolution> {
        let entries = self.entries.lock().unwrap();
        entries.values().find_map(|machine| {
            let app = machine.apps.values().find(|app| app.route.host == host)?;
            let link = machine.link.lock().unwrap();
            Some(match link.status {
                MachineStatus::Up => Resolution::Upstream(
                    SocketAddr::from((Ipv4Addr::LOCALHOST, app.local_port)),
                    Lease::new(()),
                ),
                MachineStatus::Unreachable => Resolution::Unavailable(format!(
                    "Machine {:?} is unreachable{}",
                    machine.record.name,
                    link.last_error
                        .as_ref()
                        .map_or(String::new(), |e| format!(": {e}"))
                )),
            })
        })
    }

    /// Makes a Machine-backed App and reconnects its Machine to forward to
    /// it. The caller has checked the name and hostname aren't a VM's or
    /// a VM-backed App's, and holds the VM table so neither can be taken
    /// meanwhile.
    pub(super) fn add_app(
        &self,
        request: MachineAppRequest,
        store: &Mutex<Store>,
    ) -> Result<MachineAppInfo, ApiError> {
        let mut entries = self.entries.lock().unwrap();
        if has_app(&entries, &request.name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("an App named {:?} already exists", request.name),
            ));
        }
        if let Some(owner) = app_with_host(&entries, &request.route.host) {
            return Err(super::hostname_taken(&request.route.host, &owner, false));
        }
        let machine = entries.get_mut(&request.machine).ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("no Machine named {:?}", request.machine),
            )
        })?;
        machine.apps.insert(
            request.name.clone(),
            MachineApp {
                route: request.route.clone(),
                local_port: 0,
            },
        );
        if let Err(e) = self.start_tunnel(machine) {
            machine.apps.remove(&request.name);
            return Err(internal(format!(
                "reconnect Machine {:?}: {e}",
                request.machine
            )));
        }
        let recorded = store.lock().unwrap().insert_machine_app(&MachineAppRecord {
            name: request.name.clone(),
            machine: request.machine.clone(),
            host: request.route.host.clone(),
            port: request.route.port,
        });
        if let Err(e) = recorded {
            machine.apps.remove(&request.name);
            let _ = self.start_tunnel(machine);
            return Err(internal(format!("record App {:?}: {e}", request.name)));
        }
        Ok(MachineAppInfo {
            name: request.name,
            machine: request.machine,
            route: request.route,
            status: machine.link.lock().unwrap().status,
        })
    }

    /// Removes the Machine-backed App `name` and stops forwarding to it;
    /// `false` when there's none.
    pub(super) fn remove_app(&self, name: &str, store: &Mutex<Store>) -> Result<bool, ApiError> {
        let mut entries = self.entries.lock().unwrap();
        let Some(machine) = entries.values_mut().find(|m| m.apps.contains_key(name)) else {
            return Ok(false);
        };
        store
            .lock()
            .unwrap()
            .delete_machine_app(name)
            .map_err(|e| internal(format!("remove the record of App {name:?}: {e}")))?;
        machine.apps.remove(name);
        // The App is gone either way; a failed reconnect only leaves its
        // forward open until the Machine's next change.
        if let Err(e) = self.start_tunnel(machine) {
            warn!("reconnect Machine {:?}: {e}", machine.record.name);
        }
        Ok(true)
    }

    /// The agent's SSH public key, made on first use.
    pub(super) fn public_key(&self) -> Result<String, ApiError> {
        let _making = self.keygen.lock().unwrap();
        if !self.key.exists() {
            let made = std::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "cirro-node", "-f"])
                .arg(&self.key)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| internal(format!("run ssh-keygen: {e}")))?;
            if !made.status.success() {
                return Err(internal(format!(
                    "make the Node agent's SSH key: {}",
                    String::from_utf8_lossy(&made.stderr).trim()
                )));
            }
        }
        std::fs::read_to_string(self.key.with_extension("pub"))
            .map_err(|e| internal(format!("read the Node agent's SSH public key: {e}")))
    }

    pub(super) fn list(&self) -> Vec<MachineInfo> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(Machine::info)
            .collect()
    }

    /// The fingerprint of the host key the Machine at `ssh` presents, for
    /// `cirro machine add` to show before pinning it.
    pub(super) async fn scan(&self, ssh: &str) -> Result<String, ApiError> {
        let target = Target::parse(ssh)?;
        fingerprint(&host_keys(&target).await?[0]).await
    }

    /// Pins the host key `add.host_key` names, checks the agent can log in
    /// with it, then records the Machine and connects to it.
    pub(super) async fn add(
        &self,
        add: AddMachine,
        store: &Mutex<Store>,
    ) -> Result<MachineInfo, ApiError> {
        super::validate_name("Machine", &add.name)?;
        let target = Target::parse(&add.ssh)?;
        let _adding = self.adding.lock().await;
        if self.entries.lock().unwrap().contains_key(&add.name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("a Machine named {:?} already exists", add.name),
            ));
        }
        let mut offered = Vec::new();
        let mut pinned = None;
        for key in host_keys(&target).await? {
            let fingerprint = fingerprint(&key).await?;
            if fingerprint == add.host_key {
                pinned = Some(key);
                break;
            }
            offered.push(fingerprint);
        }
        let Some(pinned_key) = pinned else {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "{target} presented no host key with fingerprint {}; it offered {} (only \
                     its Ed25519 key is checked when it has one)",
                    add.host_key,
                    offered.join(", ")
                ),
            ));
        };
        let record = MachineRecord {
            name: add.name,
            ssh: target.to_string(),
            pinned_key,
            fingerprint: add.host_key,
        };
        let public_key = self.public_key()?;
        self.write_known_hosts(&self.entries.lock().unwrap(), Some(&record))
            .map_err(|e| internal(format!("pin the host key of {}: {e}", record.name)))?;
        let refused = match ssh_command(
            &self.key,
            &self.known_hosts,
            &record.name,
            &target,
            &[],
            &["true"],
        )
        .output()
        .await
        {
            Ok(out) if out.status.success() => None,
            Ok(out) => Some(last_line(&String::from_utf8_lossy(&out.stderr))),
            Err(e) => Some(format!("run ssh: {e}")),
        };
        if let Some(why) = refused {
            let _ = self.write_known_hosts(&self.entries.lock().unwrap(), None);
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                format!(
                    "the Node agent couldn't log in to {target} ({why}); let its key in \
                     there, as {user} (best a dedicated, non-root user), with:\n  \
                     echo '{key}' >> ~/.ssh/authorized_keys",
                    user = target.user,
                    key = public_key.trim(),
                ),
            ));
        }
        store
            .lock()
            .unwrap()
            .insert_machine(&record)
            .map_err(|e| internal(format!("record Machine {:?}: {e}", record.name)))?;
        let name = record.name.clone();
        self.connect(record, BTreeMap::new())
            .map_err(|e| internal(format!("connect to Machine {name:?}: {e}")))?;
        Ok(self.entries.lock().unwrap()[&name].info())
    }

    pub(super) async fn remove(&self, name: &str, store: &Mutex<Store>) -> Result<(), ApiError> {
        // Not mid-add: rewriting known_hosts would unpin the key being added.
        let _adding = self.adding.lock().await;
        let mut entries = self.entries.lock().unwrap();
        let Some(machine) = entries.get(name) else {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("no Machine named {name:?}"),
            ));
        };
        if !machine.apps.is_empty() {
            let apps: Vec<&str> = machine.apps.keys().map(String::as_str).collect();
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "Machine {name:?} still has Apps ({}); rm them first",
                    apps.join(", ")
                ),
            ));
        }
        store
            .lock()
            .unwrap()
            .delete_machine(name)
            .map_err(|e| internal(format!("remove the record of Machine {name:?}: {e}")))?;
        entries.remove(name);
        // The record is gone, so a key left pinned here trusts nothing:
        // the next write drops it.
        let _ = self.write_known_hosts(&entries, None);
        Ok(())
    }

    /// Ends every Machine's `ssh`.
    pub(super) fn disconnect_all(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// Records the Machine with its Apps and starts its `ssh`.
    fn connect(&self, record: MachineRecord, apps: BTreeMap<String, Route>) -> io::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        self.write_known_hosts(&entries, Some(&record))?;
        let mut machine = Machine {
            record,
            apps: apps
                .into_iter()
                .map(|(name, route)| {
                    (
                        name,
                        MachineApp {
                            route,
                            local_port: 0,
                        },
                    )
                })
                .collect(),
            link: Arc::new(Mutex::new(Link {
                status: MachineStatus::Unreachable,
                last_error: None,
                host_key_changed: false,
            })),
            tunnel: tokio::spawn(async {}),
        };
        self.start_tunnel(&mut machine)?;
        entries.insert(machine.record.name.clone(), machine);
        Ok(())
    }

    /// Replaces the Machine's `ssh` with one that forwards to each of its
    /// Apps, and tracks what it says.
    fn start_tunnel(&self, machine: &mut Machine) -> io::Result<()> {
        let target = Target::parse(&machine.record.ssh).map_err(|e| io::Error::other(e.1))?;
        // Every port is chosen before anything changes, so a failure leaves
        // the running `ssh` and the ports the edge uses as they were.
        let ports = free_local_ports(machine.apps.len())?;
        let mut options = vec![
            "-N".to_string(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
        ];
        for (app, port) in machine.apps.values_mut().zip(ports) {
            app.local_port = port;
            options.push("-L".into());
            options.push(format!(
                "127.0.0.1:{}:localhost:{}",
                app.local_port, app.route.port
            ));
        }
        if machine.link.lock().unwrap().host_key_changed {
            return Ok(());
        }
        machine.tunnel.abort();
        machine.link = Arc::new(Mutex::new(Link {
            status: MachineStatus::Unreachable,
            last_error: None,
            host_key_changed: false,
        }));
        let (key, known_hosts) = (self.key.clone(), self.known_hosts.clone());
        let name = machine.record.name.clone();
        machine.tunnel = tokio::spawn(run_tunnel(
            name.clone(),
            move || ssh_command(&key, &known_hosts, &name, &target, &options, &[]),
            machine.link.clone(),
        ));
        Ok(())
    }

    /// Pins the host keys of `entries`, and of `pending` too while it's
    /// being added: each under the Machine's name, which `ssh` looks up
    /// through `HostKeyAlias` whatever address the Machine has.
    fn write_known_hosts(
        &self,
        entries: &BTreeMap<String, Machine>,
        pending: Option<&MachineRecord>,
    ) -> io::Result<()> {
        let lines: String = entries
            .values()
            .map(|m| &m.record)
            .chain(pending)
            .map(|r| format!("{} {}\n", r.name, r.pinned_key))
            .collect();
        std::fs::write(&self.known_hosts, lines)
    }
}

/// `ssh` to the Machine `name` with the agent's key (`key`), trusting only
/// its pinned host key in `known_hosts` and ignoring root's own SSH config.
fn ssh_command(
    key: &Path,
    known_hosts: &Path,
    name: &str,
    target: &Target,
    options: &[String],
    command: &[&str],
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(["-F", "/dev/null", "-T"])
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "StrictHostKeyChecking=yes"])
        .args(["-o", "GlobalKnownHostsFile=/dev/null"])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-o")
        .arg(format!("HostKeyAlias={name}"))
        .args(["-o", "IdentitiesOnly=yes"])
        .arg("-i")
        .arg(key)
        .args(["-o", "ConnectTimeout=10"])
        .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"])
        // VERBOSE, for the "Authenticated to" line that says it's up.
        .args(["-o", "LogLevel=VERBOSE"])
        .args(options)
        .args(["-p", &target.port.to_string(), "-l", &target.user, "--"])
        .arg(&target.host)
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: the closure only calls prctl(PR_SET_PDEATHSIG), which is
    // async-signal-safe and touches no locks or allocator state, so it
    // is sound between fork and exec. A crashed agent's `ssh` dies with
    // it instead of holding a tunnel nothing owns.
    unsafe {
        cmd.pre_exec(|| nix::sys::prctl::set_pdeathsig(Signal::SIGKILL).map_err(io::Error::from));
    }
    cmd
}

impl Machine {
    fn info(&self) -> MachineInfo {
        let link = self.link.lock().unwrap();
        MachineInfo {
            name: self.record.name.clone(),
            ssh: self.record.ssh.clone(),
            host_key: self.record.fingerprint.clone(),
            status: link.status,
            last_error: link.last_error.clone(),
        }
    }
}

/// How long a Machine's `ssh` waits before it's run again the first time
/// after logging in; each failed try doubles it, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// What OpenSSH says, as its last word, when the Machine's host key isn't
/// the pinned one: since known_hosts always holds the pin, this is the one
/// line that tells a changed key apart. If its wording ever changes, a
/// changed key is retried like any failure, still never trusted.
const HOST_KEY_FAILED: &str = "Host key verification failed.";

/// Keeps the Machine `name`'s `ssh` running, marking the Machine up while
/// it's logged in. A host key that doesn't match the pinned one ends it for
/// good: the Machine is never trusted again until it's removed and added.
// ponytail: retries reuse the forwards' ports; if something else took one,
// every retry fails until an App on the Machine is added or removed, which
// picks fresh ones.
async fn run_tunnel(name: String, ssh: impl Fn() -> Command, link: Arc<Mutex<Link>>) {
    let mut wait = RETRY_FIRST;
    loop {
        let ended = run_ssh(ssh(), &link).await;
        let why = if ended.host_key_changed {
            "host key changed: it isn't the one pinned when the Machine was added; if \
             that's expected, `cirro machine rm` it and add it again"
                .to_string()
        } else {
            ended.last
        };
        warn!(machine = %name, "Machine unreachable: {why}");
        *link.lock().unwrap() = Link {
            status: MachineStatus::Unreachable,
            last_error: Some(why),
            host_key_changed: ended.host_key_changed,
        };
        if ended.host_key_changed {
            return;
        }
        if ended.logged_in {
            wait = RETRY_FIRST;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
}

/// How one `ssh` went.
struct Ended {
    logged_in: bool,
    host_key_changed: bool,
    /// The last thing it said that wasn't chatter: why it ended.
    last: String,
}

/// Runs `ssh` until it exits, marking the Machine up once it has logged in.
async fn run_ssh(mut ssh: Command, link: &Mutex<Link>) -> Ended {
    let mut ended = Ended {
        logged_in: false,
        host_key_changed: false,
        last: "ssh exited".into(),
    };
    let mut child = match ssh.spawn() {
        Ok(child) => child,
        Err(e) => {
            ended.last = format!("run ssh: {e}");
            return ended;
        }
    };
    if let Some(stderr) = child.stderr.take() {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("Authenticated to ") {
                ended.logged_in = true;
                *link.lock().unwrap() = Link {
                    status: MachineStatus::Up,
                    last_error: None,
                    host_key_changed: false,
                };
            } else if line == HOST_KEY_FAILED {
                ended.host_key_changed = true;
            } else if !is_chatter(&line) {
                ended.last = line;
            }
        }
    }
    let _ = child.wait().await;
    ended
}

/// What `ssh` says at `LogLevel=VERBOSE` that isn't why it failed.
fn is_chatter(line: &str) -> bool {
    line.trim().is_empty()
        || ["Authenticated to ", "Transferred: ", "Bytes per second: "]
            .iter()
            .any(|p| line.starts_with(p))
}

/// Where a Machine's `sshd` is and who to log in as: `user@host[:port]`.
struct Target {
    user: String,
    host: String,
    port: u16,
}

impl Target {
    /// Each part becomes an `ssh` argument, so none may start with `-` or
    /// hold anything but the characters user and host names use.
    fn parse(ssh: &str) -> Result<Target, ApiError> {
        let invalid = || {
            bad_request(&format!(
                "invalid SSH target {ssh:?}: expected user@host or user@host:port"
            ))
        };
        let word = |s: &str, extra: &[u8]| {
            !s.is_empty()
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b) || extra.contains(&b))
        };
        let (user, rest) = ssh.split_once('@').ok_or_else(invalid)?;
        let (host, port) = match rest.split_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| invalid())?),
            None => (rest, 22),
        };
        if !word(user, b"_") || !word(host, b"") || port == 0 {
            return Err(invalid());
        }
        Ok(Target {
            user: user.to_string(),
            host: host.to_string(),
            port,
        })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.user, self.host)?;
        if self.port != 22 {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// The host keys the Machine presents, each as `<type> <base64>`: its
/// Ed25519 key alone when it has one. Every key type is a connection
/// dropped before login, which OpenSSH's `PerSourcePenalties` holds against
/// the Node, so asking for one keeps the login that follows from being
/// refused.
async fn host_keys(target: &Target) -> Result<Vec<String>, ApiError> {
    let mut keys = keyscan(target, &["-t", "ed25519"]).await?;
    if keys.is_empty() {
        keys = keyscan(target, &[]).await?;
    }
    if keys.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!("no SSH server answered at {target}"),
        ));
    }
    Ok(keys)
}

async fn keyscan(target: &Target, options: &[&str]) -> Result<Vec<String>, ApiError> {
    let out = Command::new("ssh-keyscan")
        .args(["-T", "5"])
        .args(options)
        .args(["-p", &target.port.to_string(), "--"])
        .arg(&target.host)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| internal(format!("run ssh-keyscan: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut fields = l.split_whitespace().skip(1);
            Some(format!("{} {}", fields.next()?, fields.next()?))
        })
        .collect())
}

/// `key`'s fingerprint as `ssh-keygen -l` prints it: `SHA256:...`.
async fn fingerprint(key: &str) -> Result<String, ApiError> {
    let mut child = Command::new("ssh-keygen")
        .args(["-l", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| internal(format!("run ssh-keygen: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{key}\n").as_bytes()).await;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| internal(format!("run ssh-keygen: {e}")))?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
        .ok_or_else(|| internal(format!("ssh-keygen couldn't read the host key {key:?}")))
}

fn has_app(entries: &BTreeMap<String, Machine>, name: &str) -> bool {
    entries.values().any(|m| m.apps.contains_key(name))
}

fn app_with_host(entries: &BTreeMap<String, Machine>, host: &str) -> Option<String> {
    entries.values().find_map(|m| {
        m.apps
            .iter()
            .find(|(_, app)| app.route.host == host)
            .map(|(name, _)| name.clone())
    })
}

/// `n` different ports free on the Node's 127.0.0.1 a moment ago: all
/// held at once while they're chosen, so none is handed out twice.
// ponytail: something else can take one before `ssh` binds it; `ssh` then
// exits (ExitOnForwardFailure) and the Machine shows unreachable until an
// App on it is added or removed (see run_tunnel).
fn free_local_ports(n: usize) -> io::Result<Vec<u16>> {
    let held = (0..n)
        .map(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
        .collect::<io::Result<Vec<_>>>()?;
    held.iter().map(|l| Ok(l.local_addr()?.port())).collect()
}

/// The last thing `ssh` said, which is why it failed when it did.
fn last_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| !is_chatter(l))
        .unwrap_or("ssh failed")
        .to_string()
}
