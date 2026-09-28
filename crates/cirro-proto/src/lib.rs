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
//! | `GET /vms/{name}/logs?offset=N`|                 | `200` console bytes      |
//! | `DELETE /vms/{name}`           |                 | `204`                    |
//!
//! `GET /vms` lists running VMs; `all=true` adds Ended VMs. The logs
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

/// Boot a VM from a rootfs that already has guest-init as `/init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// 1-32 characters of lowercase letters, digits and hyphens.
    pub name: String,
    /// Absolute path on the Node.
    pub rootfs: PathBuf,
    pub mem_mib: u32,
    pub vcpus: u8,
    /// The command guest-init runs: `command[0]` is the executable.
    pub command: Vec<String>,
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
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Graceful => "graceful",
            EndReason::Forced => "forced",
            EndReason::Exited => "exited",
            EndReason::Crashed => "crashed",
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
