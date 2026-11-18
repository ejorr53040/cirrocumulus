//! Machines: computers the Node agent reaches over SSH, which don't run the
//! agent themselves (ADR 0010). The agent logs in with a key of its own and
//! trusts only the host key pinned when the Machine was added. It keeps one
//! OpenSSH `ssh` per Machine running to know whether the Machine is up.

use super::{ApiError, bad_request, internal};
use crate::layout;
use crate::state::{MachineAppRecord, MachineRecord, Store};
use cirro_edge::{Lease, Resolution, Upstream};
use cirro_proto::{
    AddMachine, MachineAppInfo, MachineAppRequest, MachineInfo, MachineStatus, Route,
};
use hyper::StatusCode;
use nix::sys::signal::Signal;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Where each `ssh` gets a directory of its own: see `write_config`.
    ssh_runs: PathBuf,
    /// Held while the agent's key is made, so two first uses make one key.
    keygen: Mutex<()>,
    /// Held for a whole add, so two adds can't take one name or pin each
    /// other's host key.
    adding: tokio::sync::Mutex<()>,
    entries: Mutex<BTreeMap<String, Machine>>,
    store: Arc<Mutex<Store>>,
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
    /// The directory of the forward sockets this `ssh` has bound, one per
    /// App ([`socket`]); set when it's up.
    forwards: PathBuf,
}

impl Link {
    /// Before its `ssh` has logged in.
    fn down() -> Link {
        Link {
            status: MachineStatus::Unreachable,
            last_error: None,
            host_key_changed: false,
            forwards: PathBuf::new(),
        }
    }
}

impl Machines {
    pub(super) fn new(state_dir: &Path, store: Arc<Mutex<Store>>) -> Machines {
        Machines {
            key: layout::ssh_key(state_dir),
            known_hosts: layout::known_hosts(state_dir),
            ssh_runs: layout::ssh_runs(state_dir),
            keygen: Mutex::new(()),
            adding: tokio::sync::Mutex::new(()),
            entries: Mutex::new(BTreeMap::new()),
            store,
        }
    }

    /// Brings back the Machines a previous agent added, connecting to each
    /// in the background so an unreachable one holds nothing up.
    pub(super) fn restore(&self) -> io::Result<()> {
        // What a crashed agent's `ssh`s left; none of them runs any more.
        match std::fs::remove_dir_all(&self.ssh_runs) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let (records, app_records) = {
            let store = self.store.lock().unwrap();
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
        self.entries
            .lock()
            .unwrap()
            .values()
            .any(|m| m.apps.contains_key(name))
    }

    /// The Machine-backed App whose route has `host`, if any.
    pub(super) fn app_with_host(&self, host: &str) -> Option<String> {
        self.entries.lock().unwrap().values().find_map(|m| {
            m.apps
                .iter()
                .find(|(_, app)| app.route.host == host)
                .map(|(name, _)| name.clone())
        })
    }

    /// Where the edge sends a request for `host`, when a Machine-backed App
    /// has it: through its Machine's `ssh` while that's up.
    pub(super) fn resolve(&self, host: &str) -> Option<Resolution> {
        let entries = self.entries.lock().unwrap();
        entries.values().find_map(|machine| {
            let (name, _) = machine
                .apps
                .iter()
                .find(|(_, app)| app.route.host == host)?;
            let link = machine.link.lock().unwrap();
            Some(match link.status {
                MachineStatus::Up => Resolution::Upstream(
                    Upstream::Unix(socket(&link.forwards, name)),
                    Lease::new(()),
                ),
                // Without `last_error`: what `ssh` said names the Machine's
                // address, which is for `cirro machine ls`, not the edge's
                // clients.
                MachineStatus::Unreachable => Resolution::Unavailable(format!(
                    "Machine {:?} is unreachable",
                    machine.record.name
                )),
            })
        })
    }

    /// Makes a Machine-backed App and reconnects its Machine to forward to
    /// it. The caller has checked the name and hostname are free, and holds
    /// the VM table, which every add takes, so neither can be taken
    /// meanwhile.
    pub(super) fn add_app(&self, request: MachineAppRequest) -> Result<MachineAppInfo, ApiError> {
        let mut entries = self.entries.lock().unwrap();
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
            },
        );
        if let Err(e) = self.start_tunnel(machine) {
            machine.apps.remove(&request.name);
            return Err(internal(format!(
                "reconnect Machine {:?}: {e}",
                request.machine
            )));
        }
        let recorded = self
            .store
            .lock()
            .unwrap()
            .insert_machine_app(&MachineAppRecord {
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
    pub(super) fn remove_app(&self, name: &str) -> Result<bool, ApiError> {
        let mut entries = self.entries.lock().unwrap();
        let Some(machine) = entries.values_mut().find(|m| m.apps.contains_key(name)) else {
            return Ok(false);
        };
        self.store
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
            crate::cmd::run(
                std::process::Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-C", "cirro-node", "-f"])
                    .arg(&self.key),
                None,
            )
            .map_err(|e| internal(format!("make the Node agent's SSH key: {e}")))?;
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
        let target = Target::parse(ssh).map_err(bad_request)?;
        fingerprint(&host_keys(&target).await?[0]).await
    }

    /// Pins the host key `add.host_key` names, checks the agent can log in
    /// with it, then records the Machine and connects to it.
    pub(super) async fn add(&self, add: AddMachine) -> Result<MachineInfo, ApiError> {
        super::validate_name("Machine", &add.name)?;
        let target = Target::parse(&add.ssh).map_err(bad_request)?;
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
        let run = write_config(
            &self.ssh_runs,
            &self.key,
            &self.known_hosts,
            &record.name,
            &target,
            &[],
        )
        .map_err(|e| internal(format!("configure ssh for {}: {e}", record.name)))?;
        self.write_known_hosts(&self.entries.lock().unwrap(), Some(&record))
            .map_err(|e| internal(format!("pin the host key of {}: {e}", record.name)))?;
        let refused = match ssh_command(&run.dir.join(CONFIG), &record.name, &[], &["true"])
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
        self.store
            .lock()
            .unwrap()
            .insert_machine(&record)
            .map_err(|e| internal(format!("record Machine {:?}: {e}", record.name)))?;
        let name = record.name.clone();
        self.connect(record, BTreeMap::new())
            .map_err(|e| internal(format!("connect to Machine {name:?}: {e}")))?;
        Ok(self.entries.lock().unwrap()[&name].info())
    }

    pub(super) async fn remove(&self, name: &str) -> Result<(), ApiError> {
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
        self.store
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
                .map(|(name, route)| (name, MachineApp { route }))
                .collect(),
            link: Arc::new(Mutex::new(Link::down())),
            tunnel: tokio::spawn(async {}),
        };
        self.start_tunnel(&mut machine)?;
        entries.insert(machine.record.name.clone(), machine);
        Ok(())
    }

    /// Replaces the Machine's `ssh` with one that forwards to each of its
    /// Apps, and tracks what it says.
    fn start_tunnel(&self, machine: &mut Machine) -> io::Result<()> {
        let target = Target::parse(&machine.record.ssh).map_err(io::Error::other)?;
        if machine.link.lock().unwrap().host_key_changed {
            return Ok(());
        }
        let apps: Vec<(String, u16)> = machine
            .apps
            .iter()
            .map(|(name, app)| (name.clone(), app.route.port))
            .collect();
        let link = Arc::new(Mutex::new(Link::down()));
        let (key, known_hosts) = (self.key.clone(), self.known_hosts.clone());
        let (runs, name) = (self.ssh_runs.clone(), machine.record.name.clone());
        let attempt = move || -> io::Result<Attempt> {
            let run = write_config(&runs, &key, &known_hosts, &name, &target, &apps)?;
            let ssh = ssh_command(&run.dir.join(CONFIG), &name, &["-N"], &[]);
            Ok(Attempt { ssh, run })
        };
        // Before anything changes, so a failure leaves the running `ssh` and
        // the forwards the edge uses as they were.
        let first = attempt()?;
        machine.tunnel.abort();
        machine.link = link.clone();
        machine.tunnel = tokio::spawn(run_tunnel(
            machine.record.name.clone(),
            first,
            attempt,
            link,
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

/// `ssh` to the Machine `name` as `config`, its root-only config file,
/// says: which host, as whom, with which key, trusting which host key and
/// forwarding what. Its arguments, which every local user can read, name
/// no more than that file and the Machine.
fn ssh_command(config: &Path, name: &str, options: &[&str], command: &[&str]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-F")
        .arg(config)
        .args(options)
        .arg("--")
        .arg(name)
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

/// One run of a Machine's `ssh`, with the directory that holds its config
/// and forward sockets, which goes when the run does.
struct Attempt {
    ssh: Command,
    run: Run,
}

/// Keeps the Machine `name`'s `ssh` running, marking the Machine up while
/// it's logged in. A host key that doesn't match the pinned one ends it for
/// good: the Machine is never trusted again until it's removed and added.
/// Each run after `first` is a fresh `attempt`, with a directory of its own.
async fn run_tunnel(
    name: String,
    first: Attempt,
    attempt: impl Fn() -> io::Result<Attempt>,
    link: Arc<Mutex<Link>>,
) {
    let mut next = Ok(first);
    let mut wait = RETRY_FIRST;
    loop {
        // The run's sockets outlast the marking down below, so the edge is
        // never sent to one that's gone.
        let (ended, _run) = match next {
            Ok(Attempt { ssh, run }) => (run_ssh(ssh, &run, &link).await, Some(run)),
            Err(e) => (
                Ended {
                    logged_in: false,
                    host_key_changed: false,
                    last: format!("set up the forwards: {e}"),
                },
                None,
            ),
        };
        let why = if ended.host_key_changed {
            "host key changed: it isn't the one pinned when the Machine was added; if \
             that's expected, `cirro machine rm` it and add it again"
                .to_string()
        } else {
            ended.last
        };
        warn!(machine = %name, "Machine unreachable: {why}");
        {
            let mut link = link.lock().unwrap();
            link.status = MachineStatus::Unreachable;
            link.last_error = Some(why);
            link.host_key_changed = ended.host_key_changed;
        }
        if ended.host_key_changed {
            return;
        }
        if ended.logged_in {
            wait = RETRY_FIRST;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
        next = attempt();
    }
}

/// How one `ssh` went.
struct Ended {
    logged_in: bool,
    host_key_changed: bool,
    /// The last thing it said that wasn't chatter: why it ended.
    last: String,
}

/// How often a logged-in `ssh`'s forward sockets are looked for.
const BIND_POLL: Duration = Duration::from_millis(20);

/// Runs `ssh` until it exits, marking the Machine up once it has logged in
/// and bound every forward socket of `run`.
async fn run_ssh(mut ssh: Command, run: &Run, link: &Mutex<Link>) -> Ended {
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
        let mut up = false;
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    if line.starts_with("Authenticated to ") {
                        ended.logged_in = true;
                    } else if line == HOST_KEY_FAILED {
                        ended.host_key_changed = true;
                    } else if !is_chatter(&line) {
                        ended.last = without_controls(line);
                    }
                }
                // `ssh` binds its forwards after logging in and says nothing
                // when it has: until each socket is there, the edge would
                // find none to connect to.
                () = tokio::time::sleep(BIND_POLL), if ended.logged_in && !up => {
                    if run.sockets.iter().all(|s| s.exists()) {
                        up = true;
                        let mut link = link.lock().unwrap();
                        link.status = MachineStatus::Up;
                        link.last_error = None;
                        link.host_key_changed = false;
                        link.forwards.clone_from(&run.dir);
                    }
                }
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
    /// Each part goes into `ssh`'s config and `ssh-keyscan`'s arguments,
    /// so none may start with `-` or hold anything but the characters user
    /// and host names use: no space, quote or newline.
    fn parse(ssh: &str) -> Result<Target, String> {
        let invalid =
            || format!("invalid SSH target {ssh:?}: expected user@host or user@host:port");
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

/// The file in a [`Run`]'s directory that `ssh` reads instead of root's
/// own config.
const CONFIG: &str = "config";

/// Numbers the `ssh` runs' directories, so no two runs share one.
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// One `ssh`'s root-only directory: its config, and a socket for each App
/// it forwards. Removed, with all of that, when it's dropped.
struct Run {
    dir: PathBuf,
    /// Each App's forward socket, which `ssh` binds once it has logged in.
    sockets: Vec<PathBuf>,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The forward socket in `dir` for the App `app`.
fn socket(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.sock"))
}

/// Writes, in a fresh root-only directory under `runs`, the config for one
/// `ssh` to the Machine `name` at `target`: log in with the agent's `key`,
/// trust only the host key pinned in `known_hosts`, and forward a socket in
/// that directory to each of `apps` (its name and route port) on the
/// Machine's localhost. In a file, not `ssh`'s arguments, which every local
/// user can read.
// Sockets, not ports on the Node's 127.0.0.1: any local user can take a
// port, or every free one, before `ssh` binds it, and with
// ExitOnForwardFailure that keeps all the Machine's Apps down; and any
// local user can connect to a port past the edge. Only root can reach the
// directory. It's new for each run, so a socket in it is this run's.
fn write_config(
    runs: &Path,
    key: &Path,
    known_hosts: &Path,
    name: &str,
    target: &Target,
    apps: &[(String, u16)],
) -> io::Result<Run> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(runs)?;
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = runs.join(seq.to_string());
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let mut run = Run {
        dir,
        sockets: Vec::with_capacity(apps.len()),
    };
    // VERBOSE, for the "Authenticated to" line that says it has logged in.
    let mut config = format!(
        "HostName {host}\nPort {port}\nUser {user}\nHostKeyAlias {name}\n\
         IdentityFile \"{key}\"\nIdentitiesOnly yes\n\
         UserKnownHostsFile \"{known_hosts}\"\nGlobalKnownHostsFile /dev/null\n\
         StrictHostKeyChecking yes\nBatchMode yes\nRequestTTY no\nConnectTimeout 10\n\
         ServerAliveInterval 5\nServerAliveCountMax 3\nExitOnForwardFailure yes\n\
         LogLevel VERBOSE\n",
        host = target.host,
        port = target.port,
        user = target.user,
        key = key.display(),
        known_hosts = known_hosts.display(),
    );
    for (app, port) in apps {
        let path = socket(&run.dir, app);
        config += &format!("LocalForward \"{}\" localhost:{port}\n", path.display());
        run.sockets.push(path);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(run.dir.join(CONFIG))?
        .write_all(config.as_bytes())?;
    Ok(run)
}

/// The last thing `ssh` said, which is why it failed when it did.
fn last_line(stderr: &str) -> String {
    without_controls(
        stderr
            .lines()
            .rev()
            .find(|l| !is_chatter(l))
            .unwrap_or("ssh failed")
            .to_string(),
    )
}

/// `line` without control characters but tabs: the Machine's side of the
/// connection chooses what `ssh` prints, and a terminal shows it in
/// `cirro machine ls` and refusals.
fn without_controls(mut line: String) -> String {
    line.retain(|c| c == '\t' || !c.is_control());
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    /// A directory of the test's own for `ssh` runs, empty.
    fn runs(test: &str) -> PathBuf {
        let runs =
            std::env::temp_dir().join(format!("cirro-machine-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runs);
        runs
    }

    /// A run for the Machine `pi` with one App, `web`, on port 8000.
    fn pi_run(runs: &Path) -> Run {
        let target = Target::parse("pi@192.0.2.7:2222").unwrap();
        let (key, known_hosts) = (Path::new("/state/ssh_key"), Path::new("/state/known_hosts"));
        write_config(
            runs,
            key,
            known_hosts,
            "pi",
            &target,
            &[("web".into(), 8000)],
        )
        .unwrap()
    }

    /// Every local user can read `ssh`'s arguments, so they say neither
    /// where the Machine is nor who logs in with which key: the root-only
    /// config file does.
    #[test]
    fn ssh_is_told_only_its_config_file_and_the_machines_name() {
        let ssh = ssh_command(Path::new("/state/ssh/0/config"), "pi", &["-N"], &[]);
        let args: Vec<&OsStr> = ssh.as_std().get_args().collect();
        assert_eq!(
            args,
            ["-F", "/state/ssh/0/config", "-N", "--", "pi"].map(OsStr::new)
        );
    }

    /// A run's config and its forward sockets sit in a directory only its
    /// owner, root, can enter, not on ports any local user can take or
    /// reach; and they go with the run.
    #[test]
    fn a_runs_config_and_forward_sockets_are_its_owners_alone_and_go_with_it() {
        let runs = runs("perms");
        let run = pi_run(&runs);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&runs), 0o700);
        assert_eq!(mode(&run.dir), 0o700);
        assert_eq!(mode(&run.dir.join(CONFIG)), 0o600);
        let web = run.dir.join("web.sock");
        assert_eq!(run.sockets, std::slice::from_ref(&web));
        let config = std::fs::read_to_string(run.dir.join(CONFIG)).unwrap();
        for line in [
            "HostName 192.0.2.7\n",
            "Port 2222\n",
            "User pi\n",
            "IdentityFile \"/state/ssh_key\"\n",
            "HostKeyAlias pi\n",
            format!("LocalForward \"{}\" localhost:8000\n", web.display()).as_str(),
        ] {
            assert!(config.contains(line), "no {line:?} in:\n{config}");
        }
        assert!(!config.contains("127.0.0.1"), "a TCP forward:\n{config}");
        let dir = run.dir.clone();
        drop(run);
        assert!(!dir.exists(), "the run's directory outlived it");
        let _ = std::fs::remove_dir_all(&runs);
    }

    /// The Machine is up only once `ssh` has bound every forward socket,
    /// not as soon as it has logged in: in between, the edge would find no
    /// socket for the App.
    #[tokio::test]
    async fn a_machine_is_up_once_its_forwards_are_bound_not_once_logged_in() {
        let runs = runs("bind");
        let run = pi_run(&runs);
        // Stands in for `ssh`: logs in, then binds nothing; the test does.
        let mut ssh = Command::new("sh");
        ssh.args(["-c", "echo 'Authenticated to pi' >&2; sleep 2"])
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let link = Mutex::new(Link::down());
        let status = || link.lock().unwrap().status;
        let watch = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(
                status(),
                MachineStatus::Unreachable,
                "up before its forward was bound"
            );
            let _bound = std::os::unix::net::UnixListener::bind(&run.sockets[0]).unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(status(), MachineStatus::Up, "not up once it was bound");
            assert_eq!(link.lock().unwrap().forwards, run.dir);
        };
        let (ended, ()) = tokio::join!(run_ssh(ssh, &run, &link), watch);
        assert!(ended.logged_in);
        drop(run);
        let _ = std::fs::remove_dir_all(&runs);
    }

    #[test]
    fn what_ssh_said_keeps_no_control_characters() {
        assert_eq!(
            last_line("banner \x1b]0;owned\x07 \u{9b}2J\tend\nTransferred: sent 1, received 2\n"),
            "banner ]0;owned 2J\tend"
        );
    }
}
