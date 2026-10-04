mod client;
mod open_rootfs;

use cirro_image::ImageCache;
use cirro_image::run_config::merge_env;
use cirro_node::agent;
use cirro_node::release::ReleaseBinaries;
use cirro_node::subnet::Subnet;
use cirro_proto::{Route, RunRequest, Stats, StopRequest, User, VM_STATE_HEADER, VmInfo};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use hyper::Method;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Pull, list and remove cached images
    #[command(subcommand)]
    Image(ImageCommand),
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
    /// Delete an Ended VM's record and console log, or a parked VM's snapshot
    Rm { name: String },
    /// Snapshot a VM to disk and free its RAM
    Park { name: String },
    /// Start a parked VM again from its snapshot and print its VM address
    Wake { name: String },
    /// Live terminal dashboard
    Top {
        /// Print one snapshot as text and exit
        #[arg(long)]
        once: bool,
    },
    /// Time boot, park and wake on this Node, with a throwaway VM per run
    Bench(BenchArgs),
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
    /// Make the VM an App: the Node's edge sends requests for this hostname
    /// to it
    #[arg(long, requires = "port")]
    host: Option<String>,
    /// The port the App listens on inside the VM
    #[arg(long, requires = "host")]
    port: Option<u16>,
    /// Park the App once no request has come through the edge for this
    /// many seconds; the next request wakes it
    #[arg(long, value_name = "SECS", requires = "host",
        value_parser = clap::value_parser!(u32).range(1..))]
    idle_park: Option<u32>,
    /// An image (nginx:alpine, ghcr.io/owner/app@sha256:...), or the path
    /// of an ext4 rootfs with guest-init as /init
    #[arg(value_name = "IMAGE|ROOTFS")]
    image: String,
    /// The command to run in the VM, and its arguments [default: the
    /// image's]. A rootfs has no default, so needs one.
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct BenchArgs {
    /// How many times to boot, park and wake a VM
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
    runs: u32,
    /// Guest memory, e.g. 256M or 1G
    #[arg(long, default_value = "256M", value_parser = parse_mem_mib)]
    mem: u32,
    /// Guest vCPUs
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8)
        .range(i64::from(cirro_proto::MIN_VCPUS)..=i64::from(cirro_proto::MAX_VCPUS)))]
    vcpus: u8,
    /// An image, or the path of an ext4 rootfs with guest-init as /init
    #[arg(value_name = "IMAGE|ROOTFS")]
    image: String,
    /// The command each VM runs, and its arguments [default: the image's].
    /// A rootfs has no default, so needs one.
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
        /// Serve the HTTP edge on this address (e.g. 0.0.0.0:80)
        #[arg(long, value_name = "ADDRESS:PORT")]
        http: Option<SocketAddr>,
        /// Serve the HTTPS edge on this address (e.g. 0.0.0.0:443)
        #[arg(long, value_name = "ADDRESS:PORT")]
        https: Option<SocketAddr>,
        /// Get public hostnames' certificates from Let's Encrypt, with this
        /// contact address, agreeing to its terms of service
        #[arg(long, requires = "https", requires = "http")]
        acme_email: Option<String>,
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
    /// Print the CA certificate the HTTPS edge's certificates come from,
    /// for clients to trust
    Ca,
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
        /// Serve the HTTP edge, which routes requests to Apps by hostname,
        /// on this address (e.g. 0.0.0.0:80)
        #[arg(long, value_name = "ADDRESS:PORT")]
        http: Option<SocketAddr>,
        /// Serve the HTTPS edge on this address (e.g. 0.0.0.0:443), with
        /// certificates from the Node's own CA (`cirro node ca`)
        #[arg(long, value_name = "ADDRESS:PORT")]
        https: Option<SocketAddr>,
        /// Get public hostnames' certificates from an ACME CA (Let's
        /// Encrypt unless --acme-directory says otherwise), with this
        /// contact address, agreeing to the CA's terms of service. Needs
        /// the HTTP edge on port 80.
        #[arg(long, requires = "https", requires = "http")]
        acme_email: Option<String>,
        /// The ACME CA's directory URL
        #[arg(
            long,
            requires = "acme_email",
            default_value = "https://acme-v02.api.letsencrypt.org/directory"
        )]
        acme_directory: String,
        /// Trust this CA certificate (PEM) for the ACME directory, e.g. a
        /// test CA's
        #[arg(long, requires = "acme_email")]
        acme_root: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Pull an image and build its rootfs, ready for `cirro run`
    Pull {
        /// e.g. nginx:alpine or ghcr.io/owner/app@sha256:...
        image: String,
    },
    /// List cached images
    Ls,
    /// Remove a cached image
    Rm {
        /// A reference it was pulled as, or its digest or the start of one
        image: String,
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
                http,
                https,
                acme_email,
                acme_directory,
                acme_root,
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
                    http,
                    https,
                    acme: acme_email.map(|email| agent::AcmeConfig {
                        directory: acme_directory,
                        email,
                        root: acme_root,
                    }),
                })
                .await
                .map_err(|e| format!("node agent: {e}"))
            }
            Command::Run(args) => run(&cli.socket, args).await,
            Command::Bench(args) => bench(&cli.socket, args).await,
            Command::Top { once: true } => top_once(&cli.socket).await,
            Command::Top { once: false } => top(&cli.socket).await,
            Command::Image(command) => image_command(command).await,
            Command::Ps { all } => ps(&cli.socket, all).await,
            Command::Logs { follow, name } => logs(&cli.socket, &name, follow).await,
            Command::Stop {
                force,
                timeout,
                name,
            } => stop(&cli.socket, &name, force, timeout).await,
            Command::Rm { name } => rm(&cli.socket, &name).await,
            Command::Park { name } => park(&cli.socket, &name).await,
            Command::Wake { name } => wake(&cli.socket, &name).await,
            Command::Node(NodeCommand::Install {
                state_dir,
                group,
                subnet,
                unit_name,
                firecracker,
                jailer,
                kernel,
                http,
                https,
                acme_email,
            }) => cirro_image::check_mke2fs()
                .map_err(|e| io::Error::other(e.0))
                .and_then(|()| {
                    cirro_node::install::install(&cirro_node::install::InstallConfig {
                        state_dir,
                        socket: cli.socket,
                        group,
                        subnet,
                        unit_name,
                        edge: cirro_node::install::EdgeConfig {
                            http,
                            https,
                            acme_email,
                        },
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
            Command::Node(NodeCommand::Ca) => node_ca(&cli.socket).await,
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
    let route = args.host.zip(args.port).map(|(host, port)| Route {
        host,
        port,
        idle_park_secs: args.idle_park,
    });
    let request = run_request(
        args.name,
        args.mem,
        args.vcpus,
        &args.image,
        Guest {
            command: args.command,
            env: args.env,
            workdir: args.workdir,
            user: args.user,
        },
        route,
    )
    .await?;
    let vm: VmInfo = client::call(socket, Method::POST, "/vms", Some(&request))
        .await?
        .ok_or("the Node agent returned no VM")?;
    let address = vm
        .vm_address
        .ok_or("the Node agent returned no VM address")?;
    println!("{address}");
    Ok(())
}

/// What the CLI asked the guest to run, before an image's defaults fill in
/// what it left out.
struct Guest {
    command: Vec<String>,
    env: Vec<String>,
    workdir: Option<String>,
    user: Option<User>,
}

/// The request that boots `image` (an image reference or a rootfs path):
/// pulls and builds an image first, and fills in its command, env, workdir
/// and user wherever `guest` leaves them out.
async fn run_request(
    name: String,
    mem_mib: u32,
    vcpus: u8,
    image: &str,
    guest: Guest,
    route: Option<Route>,
) -> Result<RunRequest, String> {
    let (rootfs, command, env, workdir, user) = match rootfs_or_image(image)? {
        Some(rootfs) => {
            if guest.command.is_empty() {
                return Err(format!(
                    "{image} is a rootfs, which has no command of its own: give one after `--`"
                ));
            }
            (rootfs, guest.command, guest.env, guest.workdir, guest.user)
        }
        None => {
            let pulled = ImageCache::new(image_cache_dir()?)
                .pull(image, GUEST_INIT_BINARY)
                .await
                .map_err(|e| e.to_string())?;
            let config = pulled.config;
            let user = config.user.map(|(uid, gid)| User { uid, gid });
            (
                pulled.rootfs,
                config.command(&guest.command),
                merge_env(&config.env, &guest.env),
                guest.workdir.or(config.workdir),
                guest.user.or(user),
            )
        }
    };
    Ok(RunRequest {
        name,
        rootfs,
        mem_mib,
        vcpus,
        command,
        env,
        workdir,
        user,
        route,
    })
}

/// Boots, parks, wakes and removes a VM `args.runs` times, one at a time,
/// and prints each operation's p50 and p99 as this client sees them: the
/// whole request, so a boot includes the agent's grace period.
async fn bench(socket: &Path, args: BenchArgs) -> Result<(), String> {
    let template = run_request(
        String::new(),
        args.mem,
        args.vcpus,
        &args.image,
        Guest {
            command: args.command,
            env: Vec::new(),
            workdir: None,
            user: None,
        },
        None,
    )
    .await?;
    let mut times: [Vec<Duration>; BENCHED.len()] = Default::default();
    let mut failed = None;
    for i in 0..args.runs {
        let name = format!("bench-{}-{i}", std::process::id());
        let result = bench_once(socket, &template, &name).await;
        // Whatever happened, the throwaway VM goes. A running one is stopped
        // first; a parked or ended one only needs removing.
        let _ = stop(socket, &name, true, 0).await;
        if let Err(e) = rm(socket, &name).await {
            eprintln!("cirro: couldn't remove the bench VM {name:?}, so remove it yourself: {e}");
        }
        match result {
            Ok(took) => {
                for (all, took) in times.iter_mut().zip(took) {
                    all.push(took);
                }
            }
            Err(e) => {
                failed = Some(format!("run {} of {} failed: {e}", i + 1, args.runs));
                break;
            }
        }
    }
    // What finished is still worth seeing when a later run failed.
    if !times[0].is_empty() {
        print_bench(times);
    }
    failed.map_or(Ok(()), Err)
}

/// What `cirro bench` times, in the order each run does them.
const BENCHED: [&str; 3] = ["boot", "park", "wake"];

/// One row per operation: how many runs, and their p50 and p99.
fn print_bench(times: [Vec<Duration>; BENCHED.len()]) {
    println!(
        "{:<10} {:>5} {:>10} {:>10}",
        "OPERATION", "RUNS", "P50", "P99"
    );
    for (operation, mut all) in BENCHED.into_iter().zip(times) {
        all.sort();
        let ms = |d: Duration| format!("{:.1}ms", d.as_secs_f64() * 1000.0);
        println!(
            "{:<10} {:>5} {:>10} {:>10}",
            operation,
            all.len(),
            ms(nearest_rank(&all, 50)),
            ms(nearest_rank(&all, 99)),
        );
    }
}

/// How long one boot, park and wake of `name` took, in [`BENCHED`] order.
async fn bench_once(
    socket: &Path,
    template: &RunRequest,
    name: &str,
) -> Result<[Duration; BENCHED.len()], String> {
    let request = RunRequest {
        name: name.to_string(),
        ..template.clone()
    };
    let timed = |path: String, body: Option<RunRequest>| async move {
        let started = Instant::now();
        client::call::<VmInfo>(socket, Method::POST, &path, body.as_ref()).await?;
        Ok::<_, String>(started.elapsed())
    };
    let boot = timed("/vms".into(), Some(request)).await?;
    let park = timed(format!("/vms/{name}/park"), None).await?;
    let wake = timed(format!("/vms/{name}/wake"), None).await?;
    Ok([boot, park, wake])
}

/// The `p`th percentile of `sorted` (ascending, not empty), by nearest
/// rank: the smallest value at least `p`% of the values are no more than.
fn nearest_rank(sorted: &[Duration], p: usize) -> Duration {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
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

async fn image_command(command: ImageCommand) -> Result<(), String> {
    let cache = ImageCache::new(image_cache_dir()?);
    match command {
        ImageCommand::Pull { image } => {
            let pulled = cache
                .pull(&image, GUEST_INIT_BINARY)
                .await
                .map_err(|e| e.to_string())?;
            println!("{}", pulled.digest);
        }
        ImageCommand::Ls => {
            println!("{:<40} {:<19} {:>5}", "REFERENCE", "DIGEST", "SIZE");
            for image in cache.list().map_err(|e| e.to_string())? {
                let size = std::fs::metadata(&image.rootfs).map_or(0, |m| m.len());
                let size = format_mem(u32::try_from(size >> 20).unwrap_or(u32::MAX));
                let digest: String = image.digest.chars().take("sha256:".len() + 12).collect();
                let references: Vec<&str> = match image.references.as_slice() {
                    [] => vec!["-"],
                    references => references.iter().map(|r| short_reference(r)).collect(),
                };
                for reference in references {
                    println!("{reference:<40} {digest:<19} {size:>5}");
                }
            }
        }
        ImageCommand::Rm { image } => {
            let removed = cache.remove(&image).map_err(|e| e.to_string())?;
            println!("{}", removed.digest);
        }
    }
    Ok(())
}

/// `reference` as people write it: `nginx:alpine`, not
/// `docker.io/library/nginx:alpine`.
fn short_reference(reference: &str) -> &str {
    reference
        .strip_prefix("docker.io/library/")
        .or_else(|| reference.strip_prefix("docker.io/"))
        .unwrap_or(reference)
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

/// The live dashboard, until `q`. The stats refresh once a second, as
/// often as the agent samples.
async fn top(socket: &Path) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("setting up the terminal: {e}"))?;
    let result = top_loop(socket, &mut terminal).await;
    ratatui::restore();
    result
}

async fn top_loop(socket: &Path, terminal: &mut ratatui::DefaultTerminal) -> Result<(), String> {
    use ratatui::crossterm::event::{self, Event, KeyEventKind};
    let mut dashboard = cirro_tui::Dashboard::default();
    let mut next_refresh = std::time::Instant::now();
    let mut refresh_failed = false;
    // Stops run in the background (a graceful one can take its whole
    // timeout) and report back here.
    let (stopped, mut stop_results) = tokio::sync::mpsc::unbounded_channel();
    loop {
        if std::time::Instant::now() >= next_refresh {
            match client::call::<Stats>(socket, Method::GET, "/stats", None::<&()>).await {
                Ok(stats) => {
                    dashboard.set_stats(stats.unwrap_or_default());
                    if std::mem::take(&mut refresh_failed) {
                        dashboard.clear_status();
                    }
                }
                Err(e) => {
                    refresh_failed = true;
                    dashboard.set_status(e);
                }
            }
            next_refresh = std::time::Instant::now() + Duration::from_secs(1);
        }
        while let Ok((name, result)) = stop_results.try_recv() {
            dashboard.set_status(match result {
                Ok(()) => format!("stopped {name}"),
                Err(e) => format!("stopping {name}: {e}"),
            });
        }
        terminal
            .draw(|frame| dashboard.draw(frame))
            .map_err(|e| format!("drawing the dashboard: {e}"))?;
        let wait = next_refresh.saturating_duration_since(std::time::Instant::now());
        if !event::poll(wait).map_err(|e| format!("reading the terminal: {e}"))? {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|e| format!("reading the terminal: {e}"))?
        else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match dashboard.key(key.code) {
            cirro_tui::Action::None => {}
            cirro_tui::Action::Quit => return Ok(()),
            cirro_tui::Action::ShowLog(name) => {
                let path = format!("/vms/{name}/logs?offset=0");
                match client::send(socket, Method::GET, &path, None::<&()>).await {
                    Ok((_, log)) => dashboard.set_log(&name, &String::from_utf8_lossy(&log)),
                    Err(e) => dashboard.set_status(e),
                }
            }
            cirro_tui::Action::Stop(name) => {
                dashboard.set_status(format!("stopping {name}…"));
                let socket = socket.to_path_buf();
                let stopped = stopped.clone();
                tokio::spawn(async move {
                    let result = stop(&socket, &name, false, 10).await;
                    let _ = stopped.send((name, result));
                });
            }
        }
    }
}

async fn top_once(socket: &Path) -> Result<(), String> {
    let stats: Stats = client::call(socket, Method::GET, "/stats", None::<&()>)
        .await?
        .unwrap_or_default();
    print!("{}", cirro_tui::snapshot(&stats));
    Ok(())
}

async fn node_ca(socket: &Path) -> Result<(), String> {
    let (_, pem) = client::send(socket, Method::GET, "/ca", None::<&()>).await?;
    std::io::stdout()
        .write_all(&pem)
        .map_err(|e| format!("write the CA: {e}"))
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
        "{:<32} {:<15} {:>7} {:>5} {:>7}  {:<24}",
        "NAME", "VM ADDRESS", "MEMORY", "VCPUS", "UPTIME", "HOST"
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
            None => cirro_tui::duration(now.saturating_sub(vm.started_at)),
            Some(_) => "-".to_string(),
        };
        let host = vm.route.as_ref().map_or("-", |r| r.host.as_str());
        let mut row = format!(
            "{:<32} {:<15} {:>7} {:>5} {:>7}  {:<24}",
            vm.name,
            address,
            format_mem(vm.mem_mib),
            vm.vcpus,
            uptime,
            host,
        );
        if all {
            match vm.ended {
                None => row.push_str("  running"),
                Some(ended) => row.push_str(&format!(
                    "  {} {} ago",
                    ended.reason.as_str(),
                    cirro_tui::duration(now.saturating_sub(ended.at))
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

async fn park(socket: &Path, name: &str) -> Result<(), String> {
    client::call::<VmInfo>(
        socket,
        Method::POST,
        &format!("/vms/{name}/park"),
        None::<&()>,
    )
    .await
    .map(drop)
}

async fn wake(socket: &Path, name: &str) -> Result<(), String> {
    let vm: VmInfo = client::call(
        socket,
        Method::POST,
        &format!("/vms/{name}/wake"),
        None::<&()>,
    )
    .await?
    .ok_or("the Node agent answered the wake with no VM")?;
    match vm.vm_address {
        Some(address) => {
            println!("{address}");
            Ok(())
        }
        None => Err("the Node agent answered the wake with no VM address".into()),
    }
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

#[cfg(test)]
mod tests {
    use super::{GUEST_INIT_BINARY, nearest_rank};
    use std::time::Duration;

    #[test]
    fn percentiles_are_by_nearest_rank() {
        let ms = Duration::from_millis;
        assert_eq!(nearest_rank(&[ms(7)], 50), ms(7));
        assert_eq!(nearest_rank(&[ms(7)], 99), ms(7));
        // Ranks ceil(1.5) = 2 and ceil(2.97) = 3.
        let three = [ms(10), ms(20), ms(30)];
        assert_eq!(nearest_rank(&three, 50), ms(20));
        assert_eq!(nearest_rank(&three, 99), ms(30));
        let hundred: Vec<Duration> = (1..=100).map(ms).collect();
        assert_eq!(nearest_rank(&hundred, 50), ms(50));
        assert_eq!(nearest_rank(&hundred, 99), ms(99));
    }

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
