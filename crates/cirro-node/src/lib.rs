//! The Node side of Cirrocumulus: the Node agent, the VM lifecycle (Firecracker API, jailer,
//! per-VM network namespaces, cgroups), the Node-wide egress policy, SQLite state, and
//! `cirro node install`/`uninstall`.

pub mod agent;
pub mod egress;
pub mod firecracker;
pub mod install;
pub mod jailer;
pub mod metrics;
pub mod release;
pub mod rootfs_open;
mod state;
pub mod subnet;
pub mod vm;

/// A fresh, empty temp dir per test, named `<prefix>-<pid>-<n>` so parallel
/// threads and parallel test binaries never collide.
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) fn tempdir(prefix: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
