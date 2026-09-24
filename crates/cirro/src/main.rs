use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};

/// Cirrocumulus: a Firecracker mini cloud in Rust.
#[derive(Parser)]
#[command(name = "cirro", version)]
struct Cli {
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
    /// Boot a VM from an OCI image
    Run { image: String },
    /// List VMs
    Ps,
    /// Show a VM's console log
    Logs { name: String },
    /// Open a shell in a VM
    Ssh { name: String },
    /// Stop a VM
    Stop { name: String },
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
    // No subcommand is implemented yet; each milestone replaces this with a
    // typed `Cli::from_arg_matches` dispatch as its commands land.
    let matches = Cli::command().get_matches();
    let mut path = vec!["cirro"];
    let mut m = &matches;
    while let Some((name, sub)) = m.subcommand() {
        path.push(name);
        m = sub;
    }
    eprintln!("{}: not yet implemented", path.join(" "));
    ExitCode::FAILURE
}
