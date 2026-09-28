//! Types and RPC definitions shared between server, nodes and CLI.
//!
//! The CLI and the Node agent speak HTTP+JSON over the agent's Unix socket
//! (ADR 0002), so each type here is one request or response body:
//!
//! | Request                       | Body            | Success response          |
//! | ----------------------------- | --------------- | ------------------------- |
//! | `POST /vms`                   | [`RunRequest`]  | `201` [`VmInfo`]          |
//! | `GET /vms`                    |                 | `200` `[`[`VmInfo`]`]`    |
//! | `POST /vms/{name}/stop`       | [`StopRequest`] | `204`                     |
//!
//! Every non-2xx response carries an [`ErrorBody`].

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;

/// Boot a VM from a rootfs that already has guest-init as `/init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    pub name: String,
    /// Absolute path on the Node.
    pub rootfs: PathBuf,
    pub mem_mib: u32,
    pub vcpus: u8,
    /// The app guest-init runs: `command[0]` is the executable.
    pub command: Vec<String>,
}

/// A running VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub vm_address: Ipv4Addr,
    pub mem_mib: u32,
    pub vcpus: u8,
    /// Seconds since the Unix epoch, on the Node's clock.
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopRequest {
    /// Kill the VM immediately instead of asking the app to exit.
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Human-readable, printed by the CLI as-is.
    pub error: String,
}
