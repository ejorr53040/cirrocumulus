//! Where the Node keeps each thing in its state dir. The agent, install
//! and uninstall all find them here, so they can't disagree.

use std::path::{Path, PathBuf};

/// The SQLite state database: VMs, Machines and Machine-backed Apps.
pub(crate) fn state_db(state_dir: &Path) -> PathBuf {
    state_dir.join("state.db")
}

/// Console logs, one per VM start.
pub(crate) fn logs(state_dir: &Path) -> PathBuf {
    state_dir.join("logs")
}

/// Parked VMs' snapshots, one directory per name. Root-only: they hold the
/// guests' memory.
pub(crate) fn parked(state_dir: &Path) -> PathBuf {
    state_dir.join("parked")
}

/// jailer's `--chroot-base-dir`, where every VM's jail is.
pub(crate) fn jail_base(state_dir: &Path) -> PathBuf {
    state_dir.join("jail")
}

/// `net.ipv4.ip_forward` as it was before the first agent turned it on, so
/// uninstall can put it back.
pub(crate) fn ip_forward_record(state_dir: &Path) -> PathBuf {
    state_dir.join("ip_forward.before")
}

/// The agent's SSH private key; its public key is next to it with `.pub`.
pub(crate) fn ssh_key(state_dir: &Path) -> PathBuf {
    state_dir.join("ssh_key")
}

/// Every Machine's pinned host key.
pub(crate) fn known_hosts(state_dir: &Path) -> PathBuf {
    state_dir.join("known_hosts")
}

/// What install resolved, for uninstall.
pub(crate) fn node_config(state_dir: &Path) -> PathBuf {
    state_dir.join("node.json")
}

/// The pinned Firecracker release install fetched.
pub(crate) fn release(state_dir: &Path) -> PathBuf {
    state_dir.join("release")
}
