//! M3 slice 2: tap networking. Creates a persistent tap device (host side,
//! `cirro_node::network::create_persistent_tap` + `configure_link`),
//! attaches it to a booted VM's `eth0`, and checks the guest's console for
//! proof the virtio-net device was actually recognized -- not full IP
//! connectivity yet (the guest configuring its own address is
//! guest-init/vsock territory, a later slice), just that the tap attaches
//! and the guest sees a real network interface where Step 0's plain
//! rootfs (`scripts/step0/build_rootfs.sh`) otherwise only ever sees none.
//!
//! Creating a tap device needs `CAP_NET_ADMIN` -- run inside an
//! unprivileged net+user namespace (the same trick
//! `scripts/apps/uvm-career-quiz/demo.sh` uses), not as real root:
//!
//!   unshare --net --user --map-root-user -- cargo test -p cirro-node --test network
//!
//! Skipped (not failed) when not running with that capability, or when
//! `/dev/kvm` is missing, matching RESEARCH.md M3's stated test method.

use cirro_node::firecracker::{BootConfig, Client};
use cirro_node::network::{configure_link, create_persistent_tap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BOOT_MARKER: &str = "STEP0_BOOT_OK";
const ETH0_MARKER: &str = "STEP0_HAS_ETH0";
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);
const TAP_NAME: &str = "cirro-test-tap";

struct FirecrackerGuard(Child);

impl Drop for FirecrackerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_root() -> PathBuf {
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
async fn tap_attaches_and_guest_sees_eth0() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm not present");
        return;
    }

    if let Err(e) = create_persistent_tap(TAP_NAME) {
        eprintln!(
            "skipping: can't create tap device ({e}) -- rerun inside \
             `unshare --net --user --map-root-user`"
        );
        return;
    }

    let (connection, handle, _) = rtnetlink::new_connection().expect("open rtnetlink connection");
    tokio::spawn(connection);
    configure_link(&handle, TAP_NAME, "172.16.50.1".parse().unwrap(), 30)
        .await
        .expect("configure host side of the tap device");

    let repo_root = repo_root();
    let build_dir = repo_root.join("scripts/step0/.build");
    let kernel = latest_matching(&build_dir, "vmlinux-")
        .unwrap_or_else(|| panic!("no kernel image under {}", build_dir.display()));
    let rootfs = build_dir.join("rootfs.ext4");
    assert!(rootfs.exists(), "no rootfs at {}", rootfs.display());
    let firecracker_bin = repo_root.join("firecracker");

    let api_socket = build_dir.join("cirro-node-network-test.socket");
    let console_log_path = build_dir.join("cirro-node-network-test-console.log");
    let _ = std::fs::remove_file(&api_socket);
    let console_log = std::fs::File::create(&console_log_path).expect("create console log file");

    let child = Command::new(&firecracker_bin)
        .arg("--api-sock")
        .arg(&api_socket)
        .stdout(Stdio::from(
            console_log.try_clone().expect("clone console log fd"),
        ))
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
        .attach_tap("eth0", TAP_NAME)
        .await
        .expect("attach tap device");
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
            assert!(
                console.contains(ETH0_MARKER),
                "guest booted but never saw eth0; console:\n{console}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "guest never printed {BOOT_MARKER} within {BOOT_TIMEOUT:?}; console so far:\n{console}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let index = handle
        .link()
        .get()
        .match_name(TAP_NAME.to_string())
        .execute()
        .try_next()
        .await
        .expect("look up tap device to clean up")
        .map(|link| link.header.index);
    if let Some(index) = index {
        let _ = handle.link().del(index).execute().await;
    }
}

use futures_util::TryStreamExt;
