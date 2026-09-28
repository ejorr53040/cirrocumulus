//! Starting and tearing down one VM on this Node: the privileged half of
//! `cirro run` / `cirro stop`, run inside the Node agent (ADR 0002).
//!
//! [`Vm::start`] does every host-side step in order, and each step pushes
//! its undo onto a stack before the next one runs. A failure part-way
//! through unwinds that stack. A VM that started is then owned by
//! [`Vm::supervise`] until it ends, whether it stops gracefully, is forced,
//! or exits or crashes on its own. Every one of those paths unwinds the
//! same stack, so the Node is left exactly as it was found.
//!
//! Networking follows ADRs 0001 and 0003. Each VM gets its own network
//! namespace holding its tap. The guest always configures the same Guest
//! address, [`GUEST_ADDRESS`]. A veth pair joins the namespace to the Node,
//! and nftables inside the namespace does 1:1 NAT between the Guest address
//! and the VM address on the veth. The Node routes the VM address to the
//! veth, sourcing from the Node's own `.1` in the Node subnet.
//!
//! Every host object the VM owns (netns, host-side veth, jail dir, cgroup)
//! is named by [`host_id`], derived from the VM address. Names stay unique
//! on the Node while the address is held, and nothing else needs recording
//! to find them again.

use crate::firecracker::{BootConfig, Client};
use cirro_proto::EndReason;
use std::io::{Read, Seek, SeekFrom};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// The address every guest configures on `eth0` (ADR 0003).
pub const GUEST_ADDRESS: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 2);
/// The guest's gateway: the tap's address inside the VM's namespace.
const GUEST_GATEWAY: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 1);

/// The fixed vsock port guest-init reads its config on (M2 protocol).
const VSOCK_CONFIG_PORT: u32 = 52;
/// The guest's vsock context id. Any id from 3 up works; each VM has its own
/// vsock device, so every guest can use the same one.
const VSOCK_GUEST_CID: u32 = 3;
/// Where Firecracker puts the host end of the vsock device, inside the jail.
const VSOCK_SOCKET_IN_JAIL: &str = "v.sock";
/// How long guest-init has to accept its config before `start` gives up.
const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a VM must stay up after getting its config for `start` to
/// count it as started: a command that fails at once is reported by `run`
/// rather than looking like success.
const START_GRACE: Duration = Duration::from_secs(2);
/// How many console lines a failed start reports.
const CONSOLE_TAIL_LINES: usize = 10;
/// How long jailer has to bring up Firecracker's API socket.
const API_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
/// Fixed VMM overhead added to the guest's memory for the cgroup ceiling,
/// until `cirro bench` measures it.
const VMM_OVERHEAD_MIB: u64 = 32;
/// jailer places each VM's cgroup under this one.
const PARENT_CGROUP: &str = "cirro";
/// Firecracker's uid/gid is this plus the VM address's low 16 bits, so no
/// two VMs on a Node share an identity.
const VM_UID_BASE: u32 = 900_000;

/// Node-wide settings every VM start needs.
pub struct NodeConfig {
    pub firecracker: PathBuf,
    pub jailer: PathBuf,
    pub kernel: PathBuf,
    /// jailer's `--chroot-base-dir`. Must not be on a `nodev` mount, and must
    /// be short enough that jail socket paths fit in a `sockaddr_un`.
    pub jail_base: PathBuf,
    /// The Node's own address in the Node subnet (`.1`).
    pub node_address: Ipv4Addr,
}

/// What to start: the VM address is allocated by the caller.
pub struct VmSpec {
    pub vm_address: Ipv4Addr,
    pub rootfs: PathBuf,
    pub mem_mib: u32,
    pub vcpus: u8,
    pub command: Vec<String>,
    /// Where the console goes. Created (or truncated) by `start` and never
    /// removed here: it outlives the VM as its Ended VM's log.
    pub console_log: PathBuf,
}

/// A request to [`Vm::supervise`] to stop the VM.
#[derive(Debug, Clone, Copy)]
pub enum Stop {
    /// Send Ctrl-Alt-Del (guest-init turns it into SIGTERM for the command),
    /// then kill the VMM if the VM hasn't ended within `timeout`.
    Graceful { timeout: Duration },
    /// Kill the VMM now.
    Force,
}

#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// A started VM, holding everything needed to tear it down.
pub struct Vm {
    /// The VMM: jailer execs into Firecracker without forking, so this is
    /// Firecracker itself once spawned. Always the last thing started, so
    /// teardown kills it before unwinding `undo`.
    vmm: Option<Child>,
    undo: Vec<Undo>,
    api_socket: PathBuf,
    console_log: PathBuf,
}

/// The name of every host object belonging to the VM at `vm_address`, e.g.
/// `cirro-fa02` for `10.77.250.2`. Fits in `IFNAMSIZ`.
pub fn host_id(vm_address: Ipv4Addr) -> String {
    let [_, _, c, d] = vm_address.octets();
    format!("cirro-{c:02x}{d:02x}")
}

/// The cgroup jailer creates every VM's own cgroup under. jailer creates
/// it on first use; the Node agent removes it once it's empty.
pub fn parent_cgroup() -> PathBuf {
    Path::new("/sys/fs/cgroup").join(PARENT_CGROUP)
}

impl Vm {
    /// Brings up the VM described by `spec`, returning once guest-init has
    /// its config and the VM has stayed up for a 2 s grace period. On failure,
    /// everything done so far is undone first, and the error says why the
    /// VM didn't start, with the tail of its console when there is one.
    pub async fn start(node: &NodeConfig, spec: &VmSpec) -> Result<Vm, Error> {
        let mut vm = Vm {
            vmm: None,
            undo: Vec::new(),
            api_socket: PathBuf::new(),
            console_log: spec.console_log.clone(),
        };
        match vm.start_steps(node, spec).await {
            Ok(()) => Ok(vm),
            Err(e) => {
                vm.teardown().await;
                Err(e)
            }
        }
    }

    /// Owns the VM until it ends, then removes all of its host state and
    /// says why it ended: on its own (exited or crashed), or because of a
    /// [`Stop`] from `stops`. A closed `stops` counts as [`Stop::Force`].
    pub async fn supervise(mut self, mut stops: mpsc::Receiver<Stop>) -> EndReason {
        let reason = self.run_until_ended(&mut stops).await;
        self.teardown().await;
        reason
    }

    async fn run_until_ended(&mut self, stops: &mut mpsc::Receiver<Stop>) -> EndReason {
        let client = Client::new(&self.api_socket);
        let console_log = self.console_log.clone();
        let vmm = self.vmm.as_mut().expect("a started VM has a VMM");
        // `biased` so a VM that has already ended is recorded as such even
        // when a stop arrives at the same moment.
        let stop = tokio::select! {
            biased;
            status = vmm.wait() => return natural_end(status, &console_log),
            stop = stops.recv() => stop,
        };
        match stop {
            Some(Stop::Graceful { timeout }) => {
                if client.send_ctrl_alt_del().await.is_err() {
                    // Most likely the VM ended on its own just now.
                    if let Ok(Some(status)) = vmm.try_wait() {
                        return natural_end(Ok(status), &console_log);
                    }
                    kill(vmm).await;
                    return EndReason::Forced;
                }
                let deadline = tokio::time::sleep(timeout);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        biased;
                        status = vmm.wait() => return graceful_end(status, &console_log),
                        _ = &mut deadline => break,
                        stop = stops.recv() => match stop {
                            Some(Stop::Graceful { .. }) => continue,
                            Some(Stop::Force) | None => break,
                        },
                    }
                }
                kill(vmm).await;
                EndReason::Forced
            }
            Some(Stop::Force) | None => {
                kill(vmm).await;
                EndReason::Forced
            }
        }
    }

    /// Kills the VMM if it's still running, then unwinds every registered
    /// start step.
    async fn teardown(&mut self) {
        if let Some(vmm) = self.vmm.as_mut() {
            kill(vmm).await;
        }
        while let Some(step) = self.undo.pop() {
            if let Err(e) = step.run().await {
                eprintln!("cirro node: teardown: {e}");
            }
        }
    }

    async fn start_steps(&mut self, node: &NodeConfig, spec: &VmSpec) -> Result<(), Error> {
        let id = host_id(spec.vm_address);
        let [_, _, c, d] = spec.vm_address.octets();
        let uid = VM_UID_BASE + (u32::from(c) << 8 | u32::from(d));
        let vm_addr = spec.vm_address.to_string();
        let node_addr = node.node_address.to_string();

        // 1. The VM's network namespace, with its tap inside.
        ip(&["netns", "add", &id]).await?;
        self.undo.push(Undo::DeleteNetns(id.clone()));
        let uid_s = uid.to_string();
        ip_in(&id, &["link", "set", "lo", "up"]).await?;
        ip_in(
            &id,
            &[
                "tuntap", "add", "dev", "tap0", "mode", "tap", "user", &uid_s, "group", &uid_s,
            ],
        )
        .await?;
        ip_in(
            &id,
            &["addr", "add", &format!("{GUEST_GATEWAY}/30"), "dev", "tap0"],
        )
        .await?;
        ip_in(&id, &["link", "set", "tap0", "up"]).await?;

        // 2. A veth pair to the Node, and 1:1 NAT between the Guest address
        //    and the VM address inside the namespace.
        ip(&[
            "link", "add", &id, "type", "veth", "peer", "name", "veth0", "netns", &id,
        ])
        .await?;
        self.undo.push(Undo::DeleteLink(id.clone()));
        ip(&["addr", "add", &format!("{node_addr}/32"), "dev", &id]).await?;
        ip(&["link", "set", &id, "up"]).await?;
        ip_in(
            &id,
            &["addr", "add", &format!("{vm_addr}/32"), "dev", "veth0"],
        )
        .await?;
        ip_in(&id, &["link", "set", "veth0", "up"]).await?;
        ip_in(
            &id,
            &["route", "add", &format!("{node_addr}/32"), "dev", "veth0"],
        )
        .await?;
        ip_in(
            &id,
            &["route", "add", "default", "via", &node_addr, "dev", "veth0"],
        )
        .await?;
        in_netns(&id, &["sysctl", "-qw", "net.ipv4.ip_forward=1"]).await?;
        nft_in(&id, &nat_ruleset(spec.vm_address)).await?;

        // 3. The Node's route to the VM address. Deleting the veth removes it.
        ip(&[
            "route",
            "add",
            &format!("{vm_addr}/32"),
            "dev",
            &id,
            "src",
            &node_addr,
        ])
        .await?;

        // 4. jailer, in the namespace and under the VM's cgroup limits.
        let jail_dir = node.jail_base.join("firecracker").join(&id);
        let root = jail_dir.join("root");
        self.undo.push(Undo::RemoveDir(jail_dir));
        self.undo
            .push(Undo::RemoveCgroup(parent_cgroup().join(&id)));
        std::fs::create_dir_all(&node.jail_base).map_err(|e| err("create jail base dir", e))?;
        let console =
            std::fs::File::create(&self.console_log).map_err(|e| err("create console log", e))?;
        let memory_max = (u64::from(spec.mem_mib) + VMM_OVERHEAD_MIB) * 1024 * 1024;
        let cpu_max = format!("cpu.max={} 100000", u32::from(spec.vcpus) * 100_000);
        let child = Command::new(&node.jailer)
            .arg("--id")
            .arg(&id)
            .arg("--exec-file")
            .arg(&node.firecracker)
            .args(["--uid", &uid_s, "--gid", &uid_s])
            .arg("--chroot-base-dir")
            .arg(&node.jail_base)
            .arg("--netns")
            .arg(Path::new("/run/netns").join(&id))
            .args(["--cgroup-version", "2", "--parent-cgroup", PARENT_CGROUP])
            .arg("--cgroup")
            .arg(format!("memory.max={memory_max}"))
            .arg("--cgroup")
            .arg(cpu_max)
            // A backstop: if a `Vm` is ever dropped without `kill`, at least
            // the VMM doesn't outlive it.
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(
                console
                    .try_clone()
                    .map_err(|e| err("clone console fd", e))?,
            )
            .stderr(console)
            .spawn()
            .map_err(|e| err("spawn jailer", e))?;
        self.vmm = Some(child);

        let api_socket = root.join("run/firecracker.socket");
        self.api_socket = api_socket.clone();
        let deadline = Instant::now() + API_SOCKET_TIMEOUT;
        while !api_socket.exists() {
            if let Some(status) = self.vmm_exit_status() {
                return Err(Error(format!(
                    "jailer exited ({status}) before Firecracker was up; last console lines:\n{}",
                    console_tail(&self.console_log)
                )));
            }
            if Instant::now() >= deadline {
                return Err(Error(format!(
                    "Firecracker's API socket never appeared at {}",
                    api_socket.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // 5. Kernel and rootfs into the jail, then the devices.
        let kernel_in_jail = root.join("vmlinux");
        if std::fs::hard_link(&node.kernel, &kernel_in_jail).is_err() {
            std::fs::copy(&node.kernel, &kernel_in_jail)
                .map_err(|e| err("copy kernel into jail", e))?;
        }
        let rootfs_in_jail = root.join("rootfs.ext4");
        std::fs::copy(&spec.rootfs, &rootfs_in_jail).map_err(|e| {
            err(
                &format!("copy rootfs {} into jail", spec.rootfs.display()),
                e,
            )
        })?;
        std::os::unix::fs::chown(&rootfs_in_jail, Some(uid), Some(uid))
            .map_err(|e| err("chown rootfs in jail", e))?;

        let client = Client::new(&api_socket);
        client
            .attach_tap("eth0", "tap0")
            .await
            .map_err(|e| err("attach tap", e))?;
        client
            .attach_vsock(VSOCK_GUEST_CID, &format!("/{VSOCK_SOCKET_IN_JAIL}"))
            .await
            .map_err(|e| err("attach vsock", e))?;
        client
            .boot(&BootConfig {
                kernel_image_path: PathBuf::from("/vmlinux"),
                boot_args: format!(
                    "console=ttyS0 reboot=k panic=1 init=/init \
                     ip={GUEST_ADDRESS}::{GUEST_GATEWAY}:255.255.255.252::eth0:off"
                ),
                rootfs_path: PathBuf::from("/rootfs.ext4"),
                vcpu_count: spec.vcpus,
                mem_size_mib: spec.mem_mib,
            })
            .await
            .map_err(|e| err("boot", e))?;

        // 6. guest-init's config over vsock, then the grace period.
        self.deliver_config(&root.join(VSOCK_SOCKET_IN_JAIL), &spec.command)
            .await?;
        let vmm = self.vmm.as_mut().expect("the VMM was just spawned");
        match tokio::time::timeout(START_GRACE, vmm.wait()).await {
            Err(_) => Ok(()),
            Ok(status) => Err(self.ended_during_start(status)),
        }
    }

    /// Sends guest-init its config with Firecracker's host-initiated vsock
    /// handshake: connect to the jail's vsock socket, send `CONNECT <port>`,
    /// wait for `OK`, write the JSON, then close. Until guest-init is
    /// listening, Firecracker drops the connection instead of answering
    /// `OK`, so this retries from scratch until [`CONFIG_TIMEOUT`], giving up
    /// early if the VM ends first.
    async fn deliver_config(&mut self, vsock: &Path, command: &[String]) -> Result<(), Error> {
        let (exec, args) = command
            .split_first()
            .ok_or_else(|| Error("no command to run".into()))?;
        let config = format!(
            "{{\"exec\":{},\"args\":{}}}",
            json_string(exec),
            json_array(args)
        );
        let deadline = Instant::now() + CONFIG_TIMEOUT;
        let mut last_error = String::from("never tried");
        while Instant::now() < deadline {
            if let Some(status) = self.vmm_exit_status() {
                return Err(self.ended_during_start(Ok(status)));
            }
            match try_deliver_config(vsock, &config).await {
                Ok(()) => return Ok(()),
                Err(e) => last_error = e,
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(Error(format!(
            "guest-init never accepted its config within {CONFIG_TIMEOUT:?} ({last_error}), \
             so the VM was forced to stop; does the rootfs have guest-init as /init? \
             Last console lines:\n{}",
            console_tail(&self.console_log)
        )))
    }

    /// Whether the VMM process has already exited, and how. `None` while it
    /// runs, or before it's been spawned.
    fn vmm_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.vmm.as_mut()?.try_wait().ok().flatten()
    }

    fn ended_during_start(&self, status: std::io::Result<std::process::ExitStatus>) -> Error {
        let how = match natural_end(status, &self.console_log) {
            EndReason::Crashed => "crashed",
            _ => "exited on its own",
        };
        // A panic's register dump pushes its reason out of the tail.
        let panic = console_tail_bytes(&self.console_log)
            .lines()
            .rev()
            .find(|l| l.contains("Kernel panic"))
            .map(|l| format!(" ({})", l.trim()))
            .unwrap_or_default();
        Error(format!(
            "the VM {how} while starting{panic}; last console lines:\n{}",
            console_tail(&self.console_log)
        ))
    }
}

/// Why a VM ended after being asked to stop gracefully: graceful, unless it
/// crashed on the way down.
fn graceful_end(
    status: std::io::Result<std::process::ExitStatus>,
    console_log: &Path,
) -> EndReason {
    match natural_end(status, console_log) {
        EndReason::Crashed => EndReason::Crashed,
        _ => EndReason::Graceful,
    }
}

async fn kill(vmm: &mut Child) {
    let _ = vmm.start_kill();
    let _ = vmm.wait().await;
}

/// Why a VM ended when nothing asked it to. guest-init reboots the guest
/// once its command exits, which Firecracker exits cleanly on; a guest
/// kernel panic takes the same reboot path (`panic=1`), so the console is
/// what tells the two apart.
fn natural_end(status: std::io::Result<std::process::ExitStatus>, console_log: &Path) -> EndReason {
    let panicked = console_tail_bytes(console_log).contains("Kernel panic");
    match status {
        Ok(s) if s.success() && !panicked => EndReason::Exited,
        _ => EndReason::Crashed,
    }
}

/// The last [`CONSOLE_TAIL_LINES`] lines of a console log, indented, for
/// error messages.
fn console_tail(path: &Path) -> String {
    let console = console_tail_bytes(path);
    let lines: Vec<&str> = console.lines().collect();
    let tail = &lines[lines.len().saturating_sub(CONSOLE_TAIL_LINES)..];
    if tail.is_empty() {
        "  (no console output)".to_string()
    } else {
        tail.iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The end of a console log, without reading all of a long one.
fn console_tail_bytes(path: &Path) -> String {
    const TAIL_BYTES: u64 = 16 * 1024;
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One step of teardown, registered as the matching start step succeeds.
enum Undo {
    DeleteNetns(String),
    DeleteLink(String),
    RemoveDir(PathBuf),
    RemoveCgroup(PathBuf),
}

impl Undo {
    async fn run(self) -> Result<(), Error> {
        match self {
            Undo::DeleteNetns(name) => ip(&["netns", "del", &name]).await,
            Undo::DeleteLink(name) => ip(&["link", "del", &name]).await,
            Undo::RemoveDir(path) => remove_if_present(std::fs::remove_dir_all(&path), &path),
            Undo::RemoveCgroup(path) => remove_if_present(std::fs::remove_dir(&path), &path),
        }
    }
}

fn remove_if_present(result: std::io::Result<()>, path: &Path) -> Result<(), Error> {
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(err(&format!("remove {}", path.display()), e))
        }
        _ => Ok(()),
    }
}

fn nat_ruleset(vm_address: Ipv4Addr) -> String {
    format!(
        "table ip cirro {{\n\
         \tchain prerouting {{ type nat hook prerouting priority dstnat; \
         ip daddr {vm_address} dnat to {GUEST_ADDRESS}; }}\n\
         \tchain postrouting {{ type nat hook postrouting priority srcnat; \
         ip saddr {GUEST_ADDRESS} snat to {vm_address}; }}\n\
         }}\n"
    )
}

async fn try_deliver_config(vsock: &Path, config: &str) -> Result<(), String> {
    let stream = UnixStream::connect(vsock)
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = BufReader::new(stream);
    stream
        .get_mut()
        .write_all(format!("CONNECT {VSOCK_CONFIG_PORT}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut ack = String::new();
    tokio::time::timeout(Duration::from_secs(1), stream.read_line(&mut ack))
        .await
        .map_err(|_| "no vsock ack".to_string())?
        .map_err(|e| e.to_string())?;
    if !ack.starts_with("OK") {
        return Err(format!("vsock handshake answered {ack:?}"));
    }
    let stream = stream.get_mut();
    stream
        .write_all(config.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.shutdown().await.map_err(|e| e.to_string())
}

fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn json_array(items: &[String]) -> String {
    serde_json::Value::from(items.to_vec()).to_string()
}

async fn ip(args: &[&str]) -> Result<(), Error> {
    run(Command::new("ip").args(args), None).await
}

async fn ip_in(netns: &str, args: &[&str]) -> Result<(), Error> {
    run(Command::new("ip").args(["-n", netns]).args(args), None).await
}

async fn in_netns(netns: &str, argv: &[&str]) -> Result<(), Error> {
    run(
        Command::new("ip").args(["netns", "exec", netns]).args(argv),
        None,
    )
    .await
}

async fn nft_in(netns: &str, ruleset: &str) -> Result<(), Error> {
    run(
        Command::new("ip").args(["netns", "exec", netns, "nft", "-f", "-"]),
        Some(ruleset),
    )
    .await
}

/// Runs a host command to completion, folding its stderr into the error.
async fn run(cmd: &mut Command, stdin: Option<&str>) -> Result<(), Error> {
    let described = format!("{:?}", cmd.as_std());
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| err(&described, e))?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        pipe.write_all(input.as_bytes())
            .await
            .map_err(|e| err(&described, e))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| err(&described, e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error(format!(
            "{described} failed: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn err(what: &str, e: impl std::fmt::Display) -> Error {
    Error(format!("{what}: {e}"))
}
