//! M3 jailer buildout: boots a real Firecracker VM inside `jailer`'s
//! chroot + cgroup + privilege-drop jail, from Rust, mirroring
//! `scripts/step0/run_jailer.sh`'s already-proven-by-hand shape. Checks
//! the same two things that script checks: the guest boots (console shows
//! `STEP0_BOOT_OK`) and the API socket isn't owned by root (jailer's whole
//! job is dropping privileges before Firecracker runs).
//!
//! Skipped (not failed) when `/dev/kvm` is missing, or when `sudo` can't
//! run the jailer binary non-interactively (the NOPASSWD sudoers rule from
//! `scripts/step0/prereqs.sh` isn't set up), matching this project's
//! existing real-Firecracker tests' skip convention.

use cirro_node::firecracker::{BootConfig, Client};
use cirro_node::jailer::{Jail, JailerConfig};
use nix::unistd::{getgid, getuid};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const BOOT_MARKER: &str = "STEP0_BOOT_OK";
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

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

fn sudo_jailer_works_noninteractively(jailer_bin: &Path) -> bool {
    Command::new("sudo")
        .arg("-n")
        .arg(jailer_bin)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn boots_the_step0_plain_rootfs_inside_a_jail() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm not present");
        return;
    }

    let repo_root = repo_root();
    let jailer_bin = repo_root.join("jailer");
    if !sudo_jailer_works_noninteractively(&jailer_bin) {
        eprintln!(
            "skipping: `sudo -n {} --help` failed -- run scripts/step0/prereqs.sh \
             to set up the NOPASSWD sudoers rule first",
            jailer_bin.display()
        );
        return;
    }

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

    let console_log_path = build_dir.join("cirro-node-jailer-test-console.log");
    let console_log =
        std::fs::File::create(&console_log_path).expect("create console log file");

    // Same short, non-`nodev` chroot base Step 0's own jailer proof uses
    // (see run_jailer.sh) -- the API socket path inside it has to fit in a
    // sockaddr_un, and jailer mknods /dev/kvm inside the chroot, which is
    // inert on a nodev mount like /tmp.
    let chroot_base_dir =
        PathBuf::from(std::env::var("HOME").expect("HOME set")).join(".cirrocumulus-step0-jail");

    let config = JailerConfig {
        id: format!("cirro-node-jailer-test-{}", std::process::id()),
        exec_file: repo_root.join("firecracker"),
        uid: getuid().as_raw(),
        gid: getgid().as_raw(),
        chroot_base_dir,
    };

    let jail = Jail::spawn(&jailer_bin, &config, &console_log)
        .await
        .expect("spawn jailer");

    // Resources referenced over the API must already be inside the jail
    // root, per docs/jailer.md -- same as run_jailer.sh's own `cp` calls.
    let kernel_in_jail = jail.root().join(kernel.file_name().expect("kernel has a filename"));
    std::fs::copy(&kernel, &kernel_in_jail).expect("copy kernel into jail root");
    std::fs::copy(&rootfs, jail.root().join("rootfs.ext4")).expect("copy rootfs into jail root");

    let api_socket = jail.api_socket_path();
    let deadline = std::time::Instant::now() + BOOT_TIMEOUT;
    while !api_socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "firecracker API socket never appeared at {}",
            api_socket.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let socket_uid = std::fs::metadata(&api_socket)
        .expect("stat API socket")
        .uid();
    assert_ne!(
        socket_uid, 0,
        "firecracker API socket at {} is owned by root -- jailer didn't drop privileges",
        api_socket.display()
    );

    let client = Client::new(&api_socket);
    client
        .boot(&BootConfig {
            // Paths given to the Firecracker API are paths as seen *inside*
            // the chroot, not on the host.
            kernel_image_path: PathBuf::from("/").join(
                kernel_in_jail
                    .file_name()
                    .expect("kernel_in_jail has a filename"),
            ),
            boot_args: "console=ttyS0 reboot=k panic=1 init=/init".to_string(),
            rootfs_path: PathBuf::from("/rootfs.ext4"),
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
