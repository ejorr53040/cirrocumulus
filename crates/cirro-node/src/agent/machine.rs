//! Machines: computers the Node agent reaches over SSH, which don't run the
//! agent themselves (ADR 0010). The agent logs in with a key of its own and
//! trusts only the host key pinned when the Machine was added. It keeps one
//! OpenSSH `ssh` per Machine running to know whether the Machine is up.

use super::{ApiError, bad_request, internal};
use tracing::warn;
use crate::state::{MachineRecord, Store};
use cirro_proto::{AddMachine, MachineInfo, MachineStatus};
use hyper::StatusCode;
use nix::sys::signal::Signal;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinHandle;

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
    link: Arc<Mutex<Link>>,
    /// Owns the Machine's `ssh`, which is killed when the task is dropped.
    tunnel: JoinHandle<()>,
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
        let records = store.lock().unwrap().load_machines()?;
        for record in records {
            // One Machine that can't be set up mustn't stop the agent starting.
            let name = record.name.clone();
            if let Err(e) = self.connect(record) {
                warn!("restore Machine {name:?}: {e}");
            }
        }
        Ok(())
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
        let refused = match self
            .ssh(&record.name, &target, &[], &["true"])
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
        self.connect(record)
            .map_err(|e| internal(format!("connect to Machine {name:?}: {e}")))?;
        Ok(self.entries.lock().unwrap()[&name].info())
    }

    pub(super) async fn remove(&self, name: &str, store: &Mutex<Store>) -> Result<(), ApiError> {
        // Not mid-add: rewriting known_hosts would unpin the key being added.
        let _adding = self.adding.lock().await;
        let mut entries = self.entries.lock().unwrap();
        if !entries.contains_key(name) {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("no Machine named {name:?}"),
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

    /// Starts the Machine's `ssh` and tracks what it says.
    fn connect(&self, record: MachineRecord) -> io::Result<()> {
        let target = Target::parse(&record.ssh).map_err(|e| io::Error::other(e.1))?;
        let mut entries = self.entries.lock().unwrap();
        self.write_known_hosts(&entries, Some(&record))?;
        let link = Arc::new(Mutex::new(Link {
            status: MachineStatus::Unreachable,
            last_error: None,
        }));
        let tunnel = tokio::spawn(run_tunnel(
            self.ssh(&record.name, &target, &["-N"], &[]),
            link.clone(),
        ));
        entries.insert(
            record.name.clone(),
            Machine {
                record,
                link,
                tunnel,
            },
        );
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

    /// `ssh` to the Machine `name` with the agent's key, trusting only its
    /// pinned host key and ignoring root's own SSH config.
    fn ssh(&self, name: &str, target: &Target, options: &[&str], command: &[&str]) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(["-F", "/dev/null", "-T"])
            .args(["-o", "BatchMode=yes"])
            .args(["-o", "StrictHostKeyChecking=yes"])
            .args(["-o", "GlobalKnownHostsFile=/dev/null"])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .arg("-o")
            .arg(format!("HostKeyAlias={name}"))
            .args(["-o", "IdentitiesOnly=yes"])
            .arg("-i")
            .arg(&self.key)
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
            cmd.pre_exec(|| {
                nix::sys::prctl::set_pdeathsig(Signal::SIGKILL).map_err(io::Error::from)
            });
        }
        cmd
    }
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

/// Runs `ssh` until it exits, marking the Machine up once it has logged in.
// ponytail: one try; an `ssh` that exits leaves the Machine unreachable
// until the agent restarts. Reconnecting with backoff is #44.
async fn run_tunnel(mut ssh: Command, link: Arc<Mutex<Link>>) {
    let mut child = match ssh.spawn() {
        Ok(child) => child,
        Err(e) => {
            link.lock().unwrap().last_error = Some(format!("run ssh: {e}"));
            return;
        }
    };
    let mut last = None;
    if let Some(stderr) = child.stderr.take() {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("Authenticated to ") {
                *link.lock().unwrap() = Link {
                    status: MachineStatus::Up,
                    last_error: None,
                };
            } else if !line.trim().is_empty() {
                last = Some(line);
            }
        }
    }
    let _ = child.wait().await;
    *link.lock().unwrap() = Link {
        status: MachineStatus::Unreachable,
        last_error: Some(last.unwrap_or_else(|| "ssh exited".into())),
    };
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

/// The last thing `ssh` said, which is why it failed when it did.
fn last_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("ssh failed")
        .to_string()
}
