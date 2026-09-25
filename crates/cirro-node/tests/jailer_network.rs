//! M3: combines the jailer slice (`jailer.rs`) and the tap-networking
//! slice (`network.rs`) into the shape M3's exit criterion actually needs
//! -- a jailed VM with a real network interface, not two proofs that
//! happen to share a crate.
//!
//! `Jail::spawn` doesn't pass `--netns`, so the jailed Firecracker process
//! shares whichever netns `sudo jailer` itself ran in: the host's default
//! one, not an isolated one. That means the tap has to be created there
//! too, and (since `jailer` drops Firecracker to an unprivileged uid/gid
//! inside the jail) pre-owned by that uid so the privilege-dropped process
//! can actually open and attach it. `create_persistent_tap` (used by
//! `network.rs`'s own test, under `unshare --net --user
//! --map-root-user`) can't do either of those -- it needs `CAP_NET_ADMIN`
//! in the *caller's* netns, and doesn't set an owner -- so this test uses
//! `create_persistent_tap_owned_by` instead, a narrowly `sudo`-scoped `ip
//! tuntap add ... user <uid>` invocation. Assigning the tap's address and
//! bringing it up need `CAP_NET_ADMIN` too, regardless of tap ownership,
//! so `configure_link_via_sudo` goes through the same sudo escape hatch
//! rather than `network.rs`'s plain `rtnetlink`-based `configure_link`
//! (which needs `CAP_NET_ADMIN` in the caller's own netns -- fine under
//! that test's `unshare`, not fine for an unprivileged caller sharing the
//! host's default netns with a `jailer`-spawned VM).
//!
//! Skipped (not failed) when `/dev/kvm` is missing, when `sudo -n
//! <jailer>` can't run non-interactively, or when the sudoers rule below
//! isn't set up:
//!
//! ```text
//! ejorr ALL=(root) NOPASSWD: \
//!   /usr/bin/ip tuntap add dev cirro-* mode tap user *, \
//!   /usr/bin/ip tuntap del dev cirro-* mode tap, \
//!   /usr/bin/ip addr add * dev cirro-*, \
//!   /usr/bin/ip link set cirro-* up
//! ```

use cirro_node::firecracker::{BootConfig, Client};
use cirro_node::jailer::{Jail, JailerConfig};
use cirro_node::network::{
    configure_link_via_sudo, create_persistent_tap_owned_by, delete_persistent_tap,
};
use futures_util::TryStreamExt;
use nix::unistd::{getgid, getuid};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const BOOT_MARKER: &str = "STEP0_BOOT_OK";
const ETH0_MARKER: &str = "STEP0_HAS_ETH0";
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
async fn jailed_boot_attaches_tap_and_guest_sees_eth0() {
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

    let uid = getuid().as_raw();
    let tap_name = format!("cirro-{}", std::process::id());
    if let Err(e) = create_persistent_tap_owned_by(&tap_name, uid) {
        eprintln!(
            "skipping: can't create tap device owned by uid {uid} ({e}) -- add the \
             `ip tuntap`/`ip addr`/`ip link` sudoers rule documented in this test's \
             module doc to /etc/sudoers.d/cirro-tap (via visudo) first"
        );
        return;
    }

    configure_link_via_sudo(&tap_name, "172.16.60.1".parse().unwrap(), 30)
        .expect("configure host side of the tap device");

    let (connection, handle, _) = rtnetlink::new_connection().expect("open rtnetlink connection");
    tokio::spawn(connection);

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

    let console_log_path = build_dir.join("cirro-node-jailer-network-test-console.log");
    let console_log =
        std::fs::File::create(&console_log_path).expect("create console log file");

    let chroot_base_dir =
        PathBuf::from(std::env::var("HOME").expect("HOME set")).join(".cirrocumulus-step0-jail");

    let config = JailerConfig {
        // Short on purpose: the API socket path inside the jail
        // (<chroot-base>/firecracker/<id>/root/run/firecracker.socket) has
        // to fit in a sockaddr_un (~108 bytes) -- see scripts/step0's own
        // note on this. `attach_tap`'s connect failed with exactly
        // "path must be shorter than SUN_LEN" before this was shortened.
        id: format!("cn-jn-{}", std::process::id()),
        exec_file: repo_root.join("firecracker"),
        uid,
        gid: getgid().as_raw(),
        chroot_base_dir,
    };

    let jail = Jail::spawn(&jailer_bin, &config, &console_log)
        .await
        .expect("spawn jailer");

    let kernel_in_jail = jail
        .root()
        .join(kernel.file_name().expect("kernel has a filename"));
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

    let client = Client::new(&api_socket);
    client
        .attach_tap("eth0", &tap_name)
        .await
        .expect("attach tap device");
    client
        .boot(&BootConfig {
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
            assert!(
                console.contains(ETH0_MARKER),
                "guest booted inside the jail but never saw eth0; console:\n{console}"
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
        .match_name(tap_name.clone())
        .execute()
        .try_next()
        .await
        .expect("look up tap device to clean up")
        .map(|link| link.header.index);
    if let Some(index) = index {
        let _ = handle.link().del(index).execute().await;
    }
    let _ = delete_persistent_tap(&tap_name);
}
