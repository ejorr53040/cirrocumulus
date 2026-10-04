//! Types and RPC definitions shared between server, nodes and CLI.
//!
//! The CLI and the Node agent speak HTTP+JSON over the agent's Unix socket
//! (ADR 0002), so each type here is one request or response body:
//!
//! | Request                        | Body            | Success response         |
//! | ------------------------------ | --------------- | ------------------------ |
//! | `POST /vms`                    | [`RunRequest`]  | `201` [`VmInfo`]         |
//! | `GET /vms[?all=true]`          |                 | `200` `[`[`VmInfo`]`]`   |
//! | `POST /vms/{name}/stop`        | [`StopRequest`] | `200` [`VmInfo`] (ended) |
//! | `POST /vms/{name}/park`        |                 | `200` [`VmInfo`] (parked)|
//! | `POST /vms/{name}/wake`        |                 | `201` [`VmInfo`]         |
//! | `GET /vms/{name}/logs?offset=N`|                 | `200` console bytes      |
//! | `DELETE /vms/{name}`           |                 | `204`                    |
//!
//! `GET /vms` lists running VMs; `all=true` adds Ended VMs. A parked VM is
//! an Ended VM whose reason is [`EndReason::Parked`]; waking it starts a new
//! VM under its name from its snapshot. The logs
//! response is the console log from byte `offset` on (default 0), as
//! `text/plain`, with a [`VM_STATE_HEADER`] saying whether the VM is still
//! running, so a client can follow it by polling from its last offset.
//!
//! Every non-2xx response carries an [`ErrorBody`].

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;

/// Response header on the logs endpoint: `running` or `ended`.
pub const VM_STATE_HEADER: &str = "cirro-vm-state";

/// Bounds on `RunRequest.mem_mib` and `RunRequest.vcpus`. The CLI and the
/// agent both enforce them; the agent's check is the one that counts, since
/// anything on its socket can send a request the CLI never validated.
pub const MIN_MEM_MIB: u32 = 128;
/// 64 GiB: keeps one VM from claiming a cgroup ceiling that starves the rest.
pub const MAX_MEM_MIB: u32 = 65536;
pub const MIN_VCPUS: u8 = 1;
pub const MAX_VCPUS: u8 = 32;

/// Boot a VM from a rootfs that already has guest-init as `/init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// 1-32 characters of lowercase letters, digits and hyphens.
    pub name: String,
    /// Absolute path on the Node.
    pub rootfs: PathBuf,
    pub mem_mib: u32,
    pub vcpus: u8,
    /// The command guest-init runs: `command[0]` is the executable, looked up
    /// on the command's `PATH` when it has no `/`.
    pub command: Vec<String>,
    /// The command's environment, as `KEY=VALUE`. guest-init adds `PATH` and
    /// `HOME` when they're missing.
    #[serde(default)]
    pub env: Vec<String>,
    /// The absolute directory the command starts in; `/` when unset.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Who the command runs as inside the guest; root when unset.
    #[serde(default)]
    pub user: Option<User>,
    /// Makes the VM an App: the Node's edge sends requests for the route's
    /// hostname to it.
    #[serde(default)]
    pub route: Option<Route>,
}

/// An App's route: requests to the Node's edge for `host` go to `port` on
/// the App's VM. A hostname belongs to at most one App on the Node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// A DNS name in lowercase, without a trailing dot.
    pub host: String,
    pub port: u16,
}

/// A numeric user and group inside the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

/// A VM, or an Ended VM when `ended` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    /// `None` once the VM has ended and its VM address is released.
    pub vm_address: Option<Ipv4Addr>,
    pub mem_mib: u32,
    pub vcpus: u8,
    /// Seconds since the Unix epoch, on the Node's clock.
    pub started_at: u64,
    pub ended: Option<Ended>,
    /// Set for an App, and kept while it is parked or ended, until `rm`.
    #[serde(default)]
    pub route: Option<Route>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ended {
    /// Seconds since the Unix epoch, on the Node's clock.
    pub at: u64,
    pub reason: EndReason,
}

/// Why a VM ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    /// A graceful stop: its command exited after being asked to.
    Graceful,
    /// Killed by a forced stop, or a graceful stop that timed out.
    Forced,
    /// Its command exited on its own.
    Exited,
    /// The guest crashed, or the VMM died unexpectedly.
    Crashed,
    /// Snapshotted to disk by a park, to be woken later.
    Parked,
    /// It was already gone when the agent started again, so how it ended is
    /// unknown.
    #[serde(rename = "agent_down")]
    AgentDown,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Graceful => "graceful",
            EndReason::Forced => "forced",
            EndReason::Exited => "exited",
            EndReason::Crashed => "crashed",
            EndReason::Parked => "parked",
            EndReason::AgentDown => "died while the agent was down",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopRequest {
    /// Kill the VM immediately instead of asking its command to exit.
    pub force: bool,
    /// For a graceful stop: how long to wait before killing the VM
    /// anyway. The agent's default applies when unset.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Human-readable, printed by the CLI as-is.
    pub error: String,
}

/// `GET /stats`: what the Node and each running VM used, a sample a
/// second, for the last minute.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    /// Seconds since the Unix epoch on the Node's clock, as VMs'
    /// `started_at` are, so uptimes don't depend on the CLI's clock.
    #[serde(default)]
    pub now: u64,
    /// Oldest first; empty until the agent has two samples.
    pub node: Vec<NodeRates>,
    pub vms: Vec<VmStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmStats {
    pub info: VmInfo,
    /// Oldest first; empty until the agent has two samples of this VM.
    pub history: Vec<VmRates>,
}

/// What the Node used per second between two samples, and its memory now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeRates {
    /// Percent of all its cores together.
    pub cpu_percent: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub rx_per_sec: u64,
    pub tx_per_sec: u64,
}

/// What a VM used per second between two samples, and its memory now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct VmRates {
    /// Percent of one core: a VM keeping two vCPUs busy reads 200.
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub io_read_per_sec: u64,
    pub io_write_per_sec: u64,
    pub rx_per_sec: u64,
    pub tx_per_sec: u64,
}
