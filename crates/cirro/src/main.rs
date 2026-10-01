mod client;
mod open_rootfs;

use cirro_image::ImageCache;
use cirro_image::run_config::merge_env;
use cirro_node::agent;
use cirro_node::release::ReleaseBinaries;
use cirro_node::subnet::Subnet;
use cirro_proto::{RunRequest, StopRequest, User, VM_STATE_HEADER, VmInfo};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use hyper::Method;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The compiled `guest-init` binary (musl static, RESEARCH.md M2), embedded
/// so `cirro` ships as one self-contained binary: building a rootfs from an
/// image writes these bytes out as the guest's `/init`. `build.rs` builds
/// guest-init as part of building `cirro` itself, so this path always
/// exists by the time this file is compiled.
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
    /// Boot a VM from an OCI image or a rootfs and print its VM address
    Run(RunArgs),
    /// List VMs
    Ps {
        /// Also list Ended VMs, with when and why they ended
        #[arg(short, long)]
        all: bool,
    },
    /// Show a VM's console log, running or ended
    Logs {
        /// Keep printing new output until the VM ends
        #[arg(short, long)]
        follow: bool,
        name: String,
    },
    /// Open a shell in a VM
    Ssh { name: String },
    /// Stop a VM
    Stop {
        /// Kill the VM immediately
        #[arg(long)]
        force: bool,
        /// Seconds to wait for the VM to end before killing it
        #[arg(long, default_value_t = 10, conflicts_with = "force")]
        timeout: u64,
        name: String,
    },
    /// Delete an Ended VM's record and console log
    Rm { name: String },
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
    /// Internal: opens a rootfs path as the given uid/gid and sends the fd
    /// back over stdin with SCM_RIGHTS (the Node agent's rootfs
    /// credential drop, issue #14). Not for direct use.
    #[command(name = "__open-rootfs", hide = true)]
    OpenRootfs { uid: u32, gid: u32, path: PathBuf },
}

#[derive(Args)]
struct RunArgs {
    /// The VM's name
    #[arg(long)]
    name: String,
    /// Guest memory, e.g. 256M or 1G
    #[arg(long, default_value = "256M", value_parser = parse_mem_mib)]
    mem: u32,
    /// Guest vCPUs
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8)
        .range(i64::from(cirro_proto::MIN_VCPUS)..=i64::from(cirro_proto::MAX_VCPUS)))]
    vcpus: u8,
    /// Set an environment variable for the command (repeatable)
    #[arg(short, long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<String>,
    /// The directory the command starts in [default: /]
    #[arg(short, long, value_name = "DIR")]
    workdir: Option<String>,
    /// Run the command as this numeric user and group [default: 0:0]
    #[arg(short, long, value_name = "UID:GID", value_parser = parse_user)]
    user: Option<User>,
    /// An image (nginx:alpine, ghcr.io/owner/app@sha256:...), or the path
    /// of an ext4 rootfs with guest-init as /init
    #[arg(value_name = "IMAGE|ROOTFS")]
    image: String,
    /// The command to run in the VM, and its arguments [default: the
    /// image's]. A rootfs has no default, so needs one.
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Fetch and verify firecracker and jailer, check KVM and cgroup v2
    Install {
        /// Where the agent keeps jails, console logs and its own config
        #[arg(long, default_value = "/var/lib/cirro")]
        state_dir: PathBuf,
        /// The group created (if missing) and used for the agent socket
        #[arg(long, default_value = "cirro")]
        group: String,
        /// The Node subnet VM addresses come from
        #[arg(long, default_value = "10.77.0.0/24")]
        subnet: Subnet,
        /// The systemd unit's name, without `.service`
        #[arg(long, default_value = "cirro")]
        unit_name: String,
        /// Use this firecracker instead of fetching the pinned release
        #[arg(long, requires = "jailer", requires = "kernel")]
        firecracker: Option<PathBuf>,
        /// Use this jailer instead of fetching the pinned release
        #[arg(long, requires = "firecracker", requires = "kernel")]
        jailer: Option<PathBuf>,
        /// Use this guest kernel instead of fetching the pinned release
        #[arg(long, requires = "firecracker", requires = "jailer")]
        kernel: Option<PathBuf>,
    },
    /// Reverse `install`: refuses while a VM is running unless `--force`
    Uninstall {
        /// The state dir `install` was given
        #[arg(long, default_value = "/var/lib/cirro")]
        state_dir: PathBuf,
        /// Uninstall even if VMs are still running
        #[arg(long)]
        force: bool,
    },
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
    // A synchronous leaf helper, dispatched before the tokio runtime exists:
    // it neither needs nor should share it with the process it was spawned
    // to serve.
    if let Command::OpenRootfs { uid, gid, path } = &cli.command {
        return open_rootfs::run(*uid, *gid, path);
    }
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
            }) => {
                init_agent_logging();
                agent::run(agent::Config {
                    state_dir,
                    socket: cli.socket,
                    socket_group,
                    subnet,
                    firecracker,
                    jailer,
                    kernel,
                })
                .await
                .map_err(|e| format!("node agent: {e}"))
            }
            Command::Run(args) => run(&cli.socket, args).await,
            Command::Ps { all } => ps(&cli.socket, all).await,
            Command::Logs { follow, name } => logs(&cli.socket, &name, follow).await,
            Command::Stop {
                force,
                timeout,
                name,
            } => stop(&cli.socket, &name, force, timeout).await,
            Command::Rm { name } => rm(&cli.socket, &name).await,
            Command::Node(NodeCommand::Install {
                state_dir,
                group,
                subnet,
                unit_name,
                firecracker,
                jailer,
                kernel,
            }) => cirro_image::check_mke2fs()
                .map_err(|e| io::Error::other(e.0))
                .and_then(|()| {
                    cirro_node::install::install(&cirro_node::install::InstallConfig {
                        state_dir,
                        socket: cli.socket,
                        group,
                        subnet,
                        unit_name,
                        // `requires` on all three CLI args above guarantees this is
                        // never a partial combination.
                        release_override: match (firecracker, jailer, kernel) {
                            (Some(firecracker), Some(jailer), Some(kernel)) => {
                                Some(ReleaseBinaries {
                                    firecracker,
                                    jailer,
                                    kernel,
                                })
                            }
                            _ => None,
                        },
                    })
                })
                .map_err(|e| format!("node install: {e}")),
            Command::Node(NodeCommand::Uninstall { state_dir, force }) => {
                cirro_node::install::uninstall(&state_dir, force)
                    .map_err(|e| format!("node uninstall: {e}"))
            }
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

/// The Node agent logs to stderr (the journal, under systemd). `CIRRO_LOG`
/// takes an `EnvFilter` directive such as `debug`; the default is `info`.
fn init_agent_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CIRRO_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
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

async fn run(socket: &Path, args: RunArgs) -> Result<(), String> {
    let (rootfs, command, env, workdir, user) = match rootfs_or_image(&args.image)? {
        Some(rootfs) => {
            if args.command.is_empty() {
                return Err(format!(
                    "{} is a rootfs, which has no command of its own: give one after `--`",
                    args.image
                ));
            }
            (rootfs, args.command, args.env, args.workdir, args.user)
        }
        None => {
            let image = ImageCache::new(image_cache_dir()?)
                .pull(&args.image, GUEST_INIT_BINARY)
                .await
                .map_err(|e| e.to_string())?;
            let config = image.config;
            let user = config.user.map(|(uid, gid)| User { uid, gid });
            (
                image.rootfs,
                config.command(&args.command),
                merge_env(&config.env, &args.env),
                args.workdir.or(config.workdir),
                args.user.or(user),
            )
        }
    };
    let request = RunRequest {
        name: args.name,
        rootfs,
        mem_mib: args.mem,
        vcpus: args.vcpus,
        command,
        env,
        workdir,
        user,
    };
    let vm: VmInfo = client::call(socket, Method::POST, "/vms", Some(&request))
        .await?
        .ok_or("the Node agent returned no VM")?;
    let address = vm
        .vm_address
        .ok_or("the Node agent returned no VM address")?;
    println!("{address}");
    Ok(())
}

/// The rootfs `arg` names, or `None` if it's an image reference. Anything
/// at that path but a directory is a rootfs, even a device: the agent
/// refuses what isn't a regular file (#14), so the path must reach it. An
/// argument written as a path is never an image.
fn rootfs_or_image(arg: &str) -> Result<Option<PathBuf>, String> {
    let looks_like_path =
        ["/", "./", "../"].iter().any(|p| arg.starts_with(p)) || arg.ends_with(".ext4");
    match std::fs::metadata(arg) {
        Ok(meta) if meta.is_dir() => {
            if looks_like_path {
                Err(format!("{arg} is a directory, not a rootfs"))
            } else {
                Ok(None)
            }
        }
        Ok(_) => std::fs::canonicalize(arg)
            .map(Some)
            .map_err(|e| format!("rootfs {arg}: {e}")),
        Err(e) if looks_like_path => Err(format!("no rootfs at {arg}: {e}")),
        Err(_) => Ok(None),
    }
}

/// `$XDG_CACHE_HOME/cirro/images`, or `~/.cache/cirro/images`.
fn image_cache_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".cache"))
            .ok_or("neither XDG_CACHE_HOME nor HOME is set, so there's nowhere to cache images")?,
    };
    Ok(base.join("cirro").join("images"))
}

async fn ps(socket: &Path, all: bool) -> Result<(), String> {
    let path = if all { "/vms?all=true" } else { "/vms" };
    let vms: Vec<VmInfo> = client::call(socket, Method::GET, path, None::<&()>)
        .await?
        .unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut header = format!(
        "{:<32} {:<15} {:>7} {:>5} {:>7}",
        "NAME", "VM ADDRESS", "MEMORY", "VCPUS", "UPTIME"
    );
    if all {
        header.push_str("  STATUS");
    }
    println!("{header}");
    for vm in vms {
        let address = vm
            .vm_address
            .map_or_else(|| "-".to_string(), |a| a.to_string());
        let uptime = match vm.ended {
            None => format_duration(now.saturating_sub(vm.started_at)),
            Some(_) => "-".to_string(),
        };
        let mut row = format!(
            "{:<32} {:<15} {:>7} {:>5} {:>7}",
            vm.name,
            address,
            format_mem(vm.mem_mib),
            vm.vcpus,
            uptime,
        );
        if all {
            match vm.ended {
                None => row.push_str("  running"),
                Some(ended) => row.push_str(&format!(
                    "  {} {} ago",
                    ended.reason.as_str(),
                    format_duration(now.saturating_sub(ended.at))
                )),
            }
        }
        println!("{row}");
    }
    Ok(())
}

async fn logs(socket: &Path, name: &str, follow: bool) -> Result<(), String> {
    let mut offset = 0u64;
    let mut out = std::io::stdout();
    loop {
        let (headers, bytes) = client::send(
            socket,
            Method::GET,
            &format!("/vms/{name}/logs?offset={offset}"),
            None::<&()>,
        )
        .await?;
        out.write_all(&bytes)
            .and_then(|()| out.flush())
            .map_err(|e| format!("writing the log: {e}"))?;
        offset += bytes.len() as u64;
        // The agent reads the VM's state before its log, so once it says
        // ended, this read got everything.
        let ended = headers
            .get(VM_STATE_HEADER)
            .is_some_and(|state| state == "ended");
        if !follow || ended {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn stop(socket: &Path, name: &str, force: bool, timeout_secs: u64) -> Result<(), String> {
    let request = StopRequest {
        force,
        timeout_secs: Some(timeout_secs),
    };
    client::call::<VmInfo>(
        socket,
        Method::POST,
        &format!("/vms/{name}/stop"),
        Some(&request),
    )
    .await
    .map(drop)
}

async fn rm(socket: &Path, name: &str) -> Result<(), String> {
    client::call::<()>(socket, Method::DELETE, &format!("/vms/{name}"), None::<&()>)
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
        Some(mib) if (cirro_proto::MIN_MEM_MIB..=cirro_proto::MAX_MEM_MIB).contains(&mib) => {
            Ok(mib)
        }
        _ => Err(format!(
            "{s:?}: memory must be between {}M and {}M",
            cirro_proto::MIN_MEM_MIB,
            cirro_proto::MAX_MEM_MIB
        )),
    }
}

/// Parses `--env`: `KEY=VALUE` with a non-empty key.
fn parse_env(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((key, _)) if !key.is_empty() => Ok(s.to_string()),
        _ => Err(format!("{s:?} is not KEY=VALUE")),
    }
}

/// Parses `--user`: `UID:GID`, both numeric.
fn parse_user(s: &str) -> Result<User, String> {
    let (uid, gid) = s
        .split_once(':')
        .and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?)))
        .ok_or_else(|| format!("{s:?} is not a numeric UID:GID like 1000:1000"))?;
    Ok(User { uid, gid })
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
