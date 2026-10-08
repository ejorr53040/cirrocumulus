//! Shared across every real-agent integration test binary in this crate
//! (`node_agent.rs`, `node_install.rs`): staging the sudoers-approved test
//! `cirro` binary, and finding the repo root and a built kernel image. A
//! `tests/<dir>/mod.rs` isn't itself compiled as a test binary (cargo only
//! does that for a top-level `tests/*.rs`), so `mod common;` in each file
//! pulls this in without adding a phantom empty test run.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/cirro is two levels under the workspace root")
        .to_path_buf()
}

pub(crate) fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME set"))
}

/// Where the sudoers rule lets the test run `cirro` as root.
pub(crate) fn test_agent_path() -> PathBuf {
    home().join(".local/lib/cirro-test/cirro")
}

pub(crate) fn latest_kernel() -> PathBuf {
    let build_dir = repo_root().join("scripts/step0/.build");
    let mut kernels: Vec<PathBuf> = std::fs::read_dir(&build_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("vmlinux-"))
        })
        .collect();
    kernels.sort();
    kernels.pop().unwrap_or_else(|| {
        panic!(
            "no kernel image under {} -- run scripts/step0/fetch_kernel.sh first",
            build_dir.display()
        )
    })
}

/// Copies the freshly built `cirro` to the sudoers-approved path (via a
/// rename, so a half-written binary is never runnable there) and checks
/// `sudo -n` will run it, once per test run. `None` means the rule is
/// missing.
pub(crate) fn test_agent() -> Option<&'static Path> {
    static AGENT: OnceLock<Option<PathBuf>> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            let path = test_agent_path();
            std::fs::create_dir_all(path.parent().unwrap()).expect("create test agent dir");
            let staging = path.with_extension(format!("tmp-{}", std::process::id()));
            std::fs::copy(env!("CARGO_BIN_EXE_cirro"), &staging).expect("stage test agent");
            std::fs::rename(&staging, &path).expect("install test agent binary");
            let ok = std::process::Command::new("sudo")
                .arg("-n")
                .arg(&path)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            ok.then_some(path)
        })
        .as_deref()
}

/// The rootfs images the tests boot, built once per test run without root
/// (`mkfs.ext4 -d`). Every image carries the fixture commands under `/app`.
#[allow(dead_code, reason = "node_install.rs only boots guest_init")]
pub(crate) struct Rootfs {
    /// guest-init as `/init`.
    pub(crate) guest_init: PathBuf,
    /// No `/init` at all, so the guest never starts guest-init.
    pub(crate) no_init: PathBuf,
    /// A `/init` that isn't guest-init and never takes a config.
    pub(crate) wrong_init: PathBuf,
}

pub(crate) fn rootfs() -> &'static Rootfs {
    static ROOTFS: OnceLock<Rootfs> = OnceLock::new();
    ROOTFS.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(concat!(env!("CARGO_CRATE_NAME"), "-rootfs"));
        let tree = dir.join("tree");
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["proc", "sys", "dev", "app"] {
            std::fs::create_dir_all(tree.join(sub)).expect("create rootfs tree");
        }
        for fixture in [
            "http_app",
            "ignore_term",
            "exit_later",
            "probe",
            "whoami",
            "spin",
            "counter",
            "dialer",
            "chatter",
        ] {
            let status = std::process::Command::new("rustc")
                .args(["--target", "x86_64-unknown-linux-musl", "-O", "-o"])
                .arg(tree.join("app").join(fixture))
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join(format!("tests/fixtures/{fixture}.rs")),
                )
                .status()
                .expect("run rustc");
            assert!(status.success(), "building the {fixture} fixture failed");
        }
        let no_init = make_image(&tree, &dir.join("no-init.ext4"));
        std::fs::copy(tree.join("app/ignore_term"), tree.join("init"))
            .expect("copy ignore_term in as /init");
        let wrong_init = make_image(&tree, &dir.join("wrong-init.ext4"));
        let guest_init_bin = repo_root()
            .join("target/guest-init-embed/x86_64-unknown-linux-musl/release/guest-init");
        std::fs::copy(&guest_init_bin, tree.join("init")).expect("copy guest-init into tree");
        let guest_init = make_image(&tree, &dir.join("guest-init.ext4"));
        Rootfs {
            guest_init,
            no_init,
            wrong_init,
        }
    })
}

fn make_image(tree: &Path, image: &Path) -> PathBuf {
    let file = std::fs::File::create(image).expect("create rootfs image");
    file.set_len(64 * 1024 * 1024).expect("size rootfs image");
    let status = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(tree)
        .arg(image)
        .status()
        .expect("run mkfs.ext4");
    assert!(status.success(), "mkfs.ext4 failed");
    image.to_path_buf()
}
