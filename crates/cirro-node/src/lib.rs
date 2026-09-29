//! VM lifecycle on one host: Firecracker API, jailer, taps, cgroups, snapshots, metrics sampling.

pub mod agent;
pub mod egress;
pub mod firecracker;
pub mod install;
pub mod jailer;
pub mod network;
pub mod release;
pub mod rootfs_open;
mod state;
pub mod vm;

/// A fresh, empty temp dir per test, named `<prefix>-<pid>-<n>` so parallel
/// test runs (same process, different threads) and parallel test *binaries*
/// (different processes) never collide. Shared by `install`'s and
/// `release`'s own `#[cfg(test)]` modules, which each used to define an
/// identical copy of this with only their own prefix literal baked in.
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
