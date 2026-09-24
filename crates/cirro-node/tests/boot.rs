//! M3 slice 1: the smallest possible piece -- a Rust client that can drive
//! the real Firecracker API to boot a VM, mirroring the curl sequence
//! `scripts/step0/run_plain.sh` already proved works by hand:
//! `/machine-config` -> `/boot-source` -> `/drives/rootfs` ->
//! `/actions {InstanceStart}`. Uses Step 0's own plain busybox rootfs and
//! kernel (`scripts/step0/fetch_kernel.sh`, `build_rootfs.sh`) rather than
//! guest-init, since this slice is about proving the HTTP-over-Unix-socket
//! client works at all, not anything guest-init-specific.
//!
//! Skipped (not failed) when `/dev/kvm` is missing, per RESEARCH.md M3's
//! own stated test method -- matches CI environments without KVM access.

use cirro_node::firecracker::{BootConfig, Client};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BOOT_MARKER: &str = "STEP0_BOOT_OK";
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

struct FirecrackerGuard(Child);

impl Drop for FirecrackerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/cirro-node.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/cirro-node is two levels under the workspace root")
        .to_path_buf()
}

fn latest_matching(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    matches.sort();
    matches.pop()
}

#[tokio::test]
async fn boots_the_step0_plain_rootfs_to_userspace() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm not present");
        return;
    }

    let repo_root = repo_root();
    let build_dir = repo_root.join("scripts/step0/.build");
    let kernel = latest_matching(&build_dir, "vmlinux-").unwrap_or_else(|| {
        panic!(
            "no kernel image under {} -- run scripts/step0/fetch_kernel.sh first",
            build_dir.display()
        )
    });
    let rootfs = build_dir.join("rootfs.ext4");
    assert!(
        rootfs.exists(),
        "no rootfs at {} -- run scripts/step0/build_rootfs.sh first",
        rootfs.display()
    );
    let firecracker_bin = repo_root.join("firecracker");

    let api_socket = build_dir.join("cirro-node-boot-test.socket");
    let console_log_path = build_dir.join("cirro-node-boot-test-console.log");
    let _ = std::fs::remove_file(&api_socket);
    let console_log = std::fs::File::create(&console_log_path).expect("create console log file");

    let child = Command::new(&firecracker_bin)
        .arg("--api-sock")
        .arg(&api_socket)
        .stdout(Stdio::from(console_log.try_clone().expect("clone console log fd")))
        .stderr(Stdio::from(console_log))
        .spawn()
        .expect("spawn firecracker");
    let _guard = FirecrackerGuard(child);

    let deadline = std::time::Instant::now() + BOOT_TIMEOUT;
    while !api_socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "firecracker API socket never appeared at {}",
            api_socket.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let client = Client::new(&api_socket);
    client
        .boot(&BootConfig {
            kernel_image_path: kernel,
            boot_args: "console=ttyS0 reboot=k panic=1 init=/init".to_string(),
            rootfs_path: rootfs,
            vcpu_count: 1,
            mem_size_mib: 128,
        })
        .await
        .expect("boot sequence");

    let deadline = std::time::Instant::now() + BOOT_TIMEOUT;
    loop {
        let mut console = String::new();
        std::fs::File::open(&console_log_path)
            .expect("open console log")
            .read_to_string(&mut console)
            .expect("read console log");
        if console.contains(BOOT_MARKER) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "guest never printed {BOOT_MARKER} within {BOOT_TIMEOUT:?}; console so far:\n{console}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
