//! Starting and tearing down one VM on this Node: the privileged half of
//! `cirro run` / `cirro stop`, run inside the Node agent (ADR 0002).
//!
//! [`Vm::start`] does every host-side step in order, and each step pushes
//! its undo onto a stack before the next one runs. A failure part-way
//! through unwinds that stack, and [`Vm::kill`] unwinds it for a VM that
//! started, so both paths leave the Node exactly as they found it.
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
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

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
    /// Where console logs go while a VM runs.
    pub state_dir: PathBuf,
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
    undo: Vec<Undo>,
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
    /// its config. On failure, everything done so far is undone first.
    pub async fn start(node: &NodeConfig, spec: &VmSpec) -> Result<Vm, Error> {
        let mut vm = Vm { undo: Vec::new() };
        match vm.start_steps(node, spec).await {
            Ok(()) => Ok(vm),
            Err(e) => {
                vm.kill().await;
                Err(e)
            }
        }
    }

    /// Kills the VM's Firecracker process immediately and removes all of
    /// its host state.
    pub async fn kill(mut self) {
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
        let console_path = node.state_dir.join(format!("{id}.console.log"));
        self.undo.push(Undo::RemoveDir(jail_dir));
        self.undo.push(Undo::RemoveFile(console_path.clone()));
        self.undo
            .push(Undo::RemoveCgroup(parent_cgroup().join(&id)));
        std::fs::create_dir_all(&node.jail_base).map_err(|e| err("create jail base dir", e))?;
        let console =
            std::fs::File::create(&console_path).map_err(|e| err("create console log", e))?;
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
        self.undo.push(Undo::KillVmm(child));

        let api_socket = root.join("run/firecracker.socket");
        let deadline = Instant::now() + API_SOCKET_TIMEOUT;
        while !api_socket.exists() {
            if let Some(status) = self.vmm_exit_status() {
                return Err(Error(format!(
                    "jailer exited ({status}) before Firecracker was up: {}",
                    console_tail(&console_path)
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

        // 6. guest-init's config over vsock.
        deliver_config(&root.join(VSOCK_SOCKET_IN_JAIL), &spec.command).await
    }
}

impl Vm {
    /// Whether the VMM process has already exited, and how. `None` while it
    /// runs, or before it's been spawned.
    fn vmm_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.undo.iter_mut().find_map(|step| match step {
            Undo::KillVmm(child) => child.try_wait().ok().flatten(),
            _ => None,
        })
    }
}

/// The last few lines of a console log, for error messages. The log itself
/// is removed with the rest of the VM's host state.
fn console_tail(path: &Path) -> String {
    const LINES: usize = 5;
    let console = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = console.lines().collect();
    let tail = lines[lines.len().saturating_sub(LINES)..].join(" | ");
    if tail.is_empty() {
        "no console output".to_string()
    } else {
        tail
    }
}

/// One step of teardown, registered as the matching start step succeeds.
enum Undo {
    DeleteNetns(String),
    DeleteLink(String),
    RemoveDir(PathBuf),
    RemoveFile(PathBuf),
    RemoveCgroup(PathBuf),
    KillVmm(Child),
}

impl Undo {
    async fn run(self) -> Result<(), Error> {
        match self {
            Undo::DeleteNetns(name) => ip(&["netns", "del", &name]).await,
            Undo::DeleteLink(name) => ip(&["link", "del", &name]).await,
            Undo::RemoveDir(path) => remove_if_present(std::fs::remove_dir_all(&path), &path),
            Undo::RemoveFile(path) => remove_if_present(std::fs::remove_file(&path), &path),
            Undo::RemoveCgroup(path) => remove_if_present(std::fs::remove_dir(&path), &path),
            Undo::KillVmm(mut child) => {
                // jailer execs into Firecracker without forking, so this is
                // the VMM itself.
                let _ = child.start_kill();
                child
                    .wait()
                    .await
                    .map(drop)
                    .map_err(|e| err("wait for Firecracker", e))
            }
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

/// Sends guest-init its config with Firecracker's host-initiated vsock
/// handshake: connect to the jail's vsock socket, send `CONNECT <port>`,
/// wait for `OK`, write the JSON, then close. Until guest-init is
/// listening, Firecracker drops the connection instead of answering `OK`,
/// so this retries from scratch until [`CONFIG_TIMEOUT`].
async fn deliver_config(vsock: &Path, command: &[String]) -> Result<(), Error> {
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
        match try_deliver_config(vsock, &config).await {
            Ok(()) => return Ok(()),
            Err(e) => last_error = e,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error(format!(
        "guest-init never accepted its config within {CONFIG_TIMEOUT:?} ({last_error})"
    )))
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
