mod client;

use cirro_node::agent::{self, Subnet};
use cirro_proto::{RunRequest, StopRequest, VmInfo};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use hyper::Method;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

/// The compiled `guest-init` binary (musl static, RESEARCH.md M2), embedded
/// so `cirro` ships as one self-contained binary -- rootfs building (M4)
/// writes these bytes out as a new guest's `/init` rather than needing a
/// separately-installed copy lying around. `build.rs` builds guest-init as
/// part of building `cirro` itself, so this path always exists by the time
/// this file is compiled.
///
/// Unused outside tests until M4 has a rootfs builder to write it out.
#[allow(dead_code)]
pub(crate) static GUEST_INIT_BINARY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/guest-init-embed/x86_64-unknown-linux-musl/release/guest-init"
));

/// Where the CLI finds the Node agent unless told otherwise.
const DEFAULT_SOCKET: &str = "/run/cirro/agent.sock";

/// Cirrocumulus: a Firecracker mini cloud in Rust.
#[derive(Parser)]
#[command(name = "cirro", version)]
struct Cli {
    /// The Node agent's socket
    #[arg(long, global = true, env = "CIRRO_SOCKET", default_value = DEFAULT_SOCKET)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up or join a node
    #[command(subcommand)]
    Node(NodeCommand),
    /// Manage the control plane
    #[command(subcommand)]
    Server(ServerCommand),
    /// Boot a VM from a rootfs image and print its VM address
    Run {
        /// The VM's name
        #[arg(long)]
        name: String,
        /// Guest memory, e.g. 256M or 1G
        #[arg(long, default_value = "256M", value_parser = parse_mem_mib)]
        mem: u32,
        /// Guest vCPUs
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=32))]
        vcpus: u8,
        /// An ext4 rootfs with guest-init as /init
        rootfs: PathBuf,
        /// The command to run in the VM, and its arguments
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List VMs
    Ps,
    /// Show a VM's console log
    Logs { name: String },
    /// Open a shell in a VM
    Ssh { name: String },
    /// Stop a VM
    Stop {
        /// Kill the VM immediately
        #[arg(long)]
        force: bool,
        name: String,
    },
    /// Snapshot a VM to disk and free its RAM
    Park { name: String },
    /// Restore a parked VM
    Wake { name: String },
    /// Live terminal dashboard
    Top,
    /// Measure boot, park and wake times on this node
    Bench,
    /// Manage an app's SQLite database
    #[command(subcommand)]
    Db(DbCommand),
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Fetch and verify firecracker and jailer, check KVM and cgroup v2
    Install,
    /// Join this node to a cluster
    Join { token: String },
    /// Run the Node agent in the foreground (as root)
    Agent {
        /// Where the agent keeps jails and console logs
        #[arg(long)]
        state_dir: PathBuf,
        /// The group allowed to use the socket: a name or a numeric gid
        #[arg(long, default_value = "cirro")]
        socket_group: String,
        /// The Node subnet VM addresses come from
        #[arg(long, default_value = "10.77.0.0/24")]
        subnet: Subnet,
        #[arg(long)]
        firecracker: PathBuf,
        #[arg(long)]
        jailer: PathBuf,
        /// The guest kernel every VM boots
        #[arg(long)]
        kernel: PathBuf,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Create the CA, state database and admin token
    Init,
}

#[derive(Subcommand)]
enum DbCommand {
    /// Restore an app's database to a point in time
    Restore {
        app: String,
        #[arg(long)]
        to: String,
    },
}

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    let runtime = tokio::runtime::Runtime::new().expect("start the tokio runtime");
    let result = runtime.block_on(async {
        match cli.command {
            Command::Node(NodeCommand::Agent {
                state_dir,
                socket_group,
                subnet,
                firecracker,
                jailer,
                kernel,
            }) => agent::run(agent::Config {
                state_dir,
                socket: cli.socket,
                socket_group,
                subnet,
                firecracker,
                jailer,
                kernel,
            })
            .await
            .map_err(|e| format!("node agent: {e}")),
            Command::Run {
                name,
                mem,
                vcpus,
                rootfs,
                command,
            } => run(&cli.socket, name, mem, vcpus, rootfs, command).await,
            Command::Ps => ps(&cli.socket).await,
            Command::Stop { force, name } => stop(&cli.socket, &name, force).await,
            _ => Err(format!("{}: not yet implemented", command_path(&matches))),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cirro: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `cirro node install`-style path of the subcommand that was invoked.
fn command_path(matches: &clap::ArgMatches) -> String {
    let mut path = vec!["cirro"];
    let mut m = matches;
    while let Some((name, sub)) = m.subcommand() {
        path.push(name);
        m = sub;
    }
    path.join(" ")
}

async fn run(
    socket: &Path,
    name: String,
    mem_mib: u32,
    vcpus: u8,
    rootfs: PathBuf,
    command: Vec<String>,
) -> Result<(), String> {
    let rootfs =
        std::fs::canonicalize(&rootfs).map_err(|e| format!("rootfs {}: {e}", rootfs.display()))?;
    let request = RunRequest {
        name,
        rootfs,
        mem_mib,
        vcpus,
        command,
    };
    let vm: VmInfo = client::call(socket, Method::POST, "/vms", Some(&request))
        .await?
        .ok_or("the Node agent returned no VM")?;
    println!("{}", vm.vm_address);
    Ok(())
}

async fn ps(socket: &Path) -> Result<(), String> {
    let vms: Vec<VmInfo> = client::call(socket, Method::GET, "/vms", None::<&()>)
        .await?
        .unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    println!(
        "{:<32} {:<15} {:>7} {:>5} {:>7}",
        "NAME", "VM ADDRESS", "MEMORY", "VCPUS", "UPTIME"
    );
    for vm in vms {
        println!(
            "{:<32} {:<15} {:>7} {:>5} {:>7}",
            vm.name,
            vm.vm_address.to_string(),
            format_mem(vm.mem_mib),
            vm.vcpus,
            format_duration(now.saturating_sub(vm.started_at)),
        );
    }
    Ok(())
}

async fn stop(socket: &Path, name: &str, force: bool) -> Result<(), String> {
    client::call::<()>(
        socket,
        Method::POST,
        &format!("/vms/{name}/stop"),
        Some(&StopRequest { force }),
    )
    .await
    .map(drop)
}

/// Parses `--mem`: a count of MiB with an optional `M`/`G` suffix.
fn parse_mem_mib(s: &str) -> Result<u32, String> {
    let lower = s.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches("ib").trim_end_matches('b');
    let (digits, scale) = match trimmed.strip_suffix('g') {
        Some(d) => (d, 1024),
        None => (trimmed.strip_suffix('m').unwrap_or(trimmed), 1),
    };
    let n: u32 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a memory size like 256M or 1G"))?;
    match n.checked_mul(scale) {
        Some(mib) if mib >= 128 => Ok(mib),
        _ => Err(format!("{s:?}: memory must be at least 128M")),
    }
}

fn format_mem(mib: u32) -> String {
    if mib.is_multiple_of(1024) {
        format!("{}G", mib / 1024)
    } else {
        format!("{mib}M")
    }
}

fn format_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::GUEST_INIT_BINARY;

    #[test]
    fn embeds_a_real_guest_init_elf_binary() {
        assert!(
            GUEST_INIT_BINARY.len() > 100_000,
            "suspiciously small for a static Rust binary: {} bytes",
            GUEST_INIT_BINARY.len()
        );
        assert_eq!(&GUEST_INIT_BINARY[..4], b"\x7fELF", "not an ELF binary");
    }
}
