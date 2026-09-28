//! M3's one seam: a throwaway Node agent under `sudo -n`, with its own
//! state dir, socket and Node subnet, driven only through the `cirro` CLI.
//! Every test ends by asserting the Node holds no Cirrocumulus state.
//!
//! Skipped (not failed) when `/dev/kvm` is missing, or when the test
//! agent's sudoers rule isn't set up. The rule names a fixed path the
//! freshly built `cirro` is copied to before each run:
//!
//! ```text
//! ejorr ALL=(root) NOPASSWD: /home/ejorr/.local/lib/cirro-test/cirro
//! ```
//!
//! That path is user-writable, so like the jailer rule this is effectively
//! passwordless root for anything running as you.
//!
//! Needs the real-Firecracker assets the `cirro-node` tests use: the
//! `firecracker` and `jailer` binaries at the repo root and a kernel under
//! `scripts/step0/.build` (`scripts/step0/fetch_kernel.sh`).

use assert_cmd::Command;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getgid};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

const SUBNET: &str = "10.77.250.0/24";
const SUBNET_PREFIX: &str = "10.77.250.";
const HTTP_PORT: u16 = 8080;
const TIMEOUT: Duration = Duration::from_secs(10);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/cirro is two levels under the workspace root")
        .to_path_buf()
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME set"))
}

/// Where the sudoers rule lets the test run `cirro` as root.
fn test_agent_path() -> PathBuf {
    home().join(".local/lib/cirro-test/cirro")
}

fn latest_kernel() -> PathBuf {
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
/// `sudo -n` will run it. `None` means the rule is missing.
fn install_test_agent() -> Option<PathBuf> {
    let path = test_agent_path();
    std::fs::create_dir_all(path.parent().unwrap()).expect("create test agent dir");
    let staging = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::copy(env!("CARGO_BIN_EXE_cirro"), &staging).expect("stage test agent binary");
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
}

/// A rootfs with guest-init as `/init` and the HTTP fixture as
/// `/app/http_app`, built without root via `mkfs.ext4 -d`.
fn build_http_rootfs(dir: &Path) -> PathBuf {
    let tree = dir.join("rootfs-tree");
    for sub in ["proc", "sys", "dev", "app"] {
        std::fs::create_dir_all(tree.join(sub)).expect("create rootfs tree");
    }
    let guest_init =
        repo_root().join("target/guest-init-embed/x86_64-unknown-linux-musl/release/guest-init");
    std::fs::copy(&guest_init, tree.join("init")).expect("copy guest-init into rootfs tree");

    let status = std::process::Command::new("rustc")
        .args(["--target", "x86_64-unknown-linux-musl", "-O", "-o"])
        .arg(tree.join("app/http_app"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http_app.rs"))
        .status()
        .expect("run rustc");
    assert!(status.success(), "building the HTTP fixture failed");

    let image = dir.join("rootfs.ext4");
    let file = std::fs::File::create(&image).expect("create rootfs image");
    file.set_len(32 * 1024 * 1024).expect("size rootfs image");
    let status = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(&tree)
        .arg(&image)
        .status()
        .expect("run mkfs.ext4");
    assert!(status.success(), "mkfs.ext4 failed");
    image
}

/// A throwaway Node agent. Dropping it stops the agent and removes its
/// state dir, so a failed assertion doesn't strand anything.
struct Agent {
    sudo: Child,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Agent {
    fn start(agent_bin: &Path, state_dir: PathBuf) -> Agent {
        let socket = state_dir.join("agent.sock");
        let repo = repo_root();
        let sudo = std::process::Command::new("sudo")
            .arg("-n")
            .arg(agent_bin)
            .args(["node", "agent", "--state-dir"])
            .arg(&state_dir)
            .arg("--socket")
            .arg(&socket)
            .args(["--socket-group", &getgid().as_raw().to_string()])
            .args(["--subnet", SUBNET])
            .arg("--firecracker")
            .arg(repo.join("firecracker"))
            .arg("--jailer")
            .arg(repo.join("jailer"))
            .arg("--kernel")
            .arg(latest_kernel())
            .spawn()
            .expect("start the Node agent");
        let agent = Agent {
            sudo,
            state_dir,
            socket,
        };
        let deadline = Instant::now() + TIMEOUT;
        while !agent.socket.exists() {
            assert!(
                Instant::now() < deadline,
                "the Node agent never created its socket at {}",
                agent.socket.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        agent
    }

    fn cirro(&self) -> Command {
        let mut cmd = Command::cargo_bin("cirro").unwrap();
        cmd.env("CIRRO_SOCKET", &self.socket);
        cmd
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // sudo relays SIGTERM to the agent it started.
        let _ = kill(Pid::from_raw(self.sudo.id() as i32), Signal::SIGTERM);
        let _ = self.sudo.wait();
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

fn http_get(address: &str, port: u16) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(
        &format!("{address}:{port}").parse().unwrap(),
        Duration::from_secs(1),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(stream, "GET / HTTP/1.0\r\nHost: {address}\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

/// Retries until the fixture answers: `run` returns once guest-init has its
/// config, a moment before the fixture itself is listening.
fn wait_for_http(address: &str, port: u16) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match http_get(address, port) {
            Ok(response) if !response.is_empty() => return response,
            result => assert!(
                Instant::now() < deadline,
                "nothing answered at http://{address}:{port} within {TIMEOUT:?}: {result:?}"
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stdout_of(args: &[&str]) -> String {
    let output = std::process::Command::new(args[0])
        .args(&args[1..])
        .output()
        .expect("run host inspection command");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Everything a VM could leave on the Node, as seen from outside the agent.
fn assert_no_cirro_state(state_dir: &Path) {
    let netns = stdout_of(&["ip", "netns", "list"]);
    assert!(
        !netns.lines().any(|l| l.starts_with("cirro-")),
        "leftover network namespaces:\n{netns}"
    );
    let links = stdout_of(&["ip", "-o", "link", "show"]);
    assert!(!links.contains("cirro-"), "leftover links:\n{links}");
    let routes = stdout_of(&["ip", "route", "show"]);
    assert!(
        !routes.contains(SUBNET_PREFIX),
        "leftover routes into {SUBNET}:\n{routes}"
    );
    let jails: Vec<_> = std::fs::read_dir(state_dir.join("jail/firecracker"))
        .into_iter()
        .flatten()
        .collect();
    assert!(jails.is_empty(), "leftover jail dirs: {jails:?}");
    let cgroups: Vec<_> = std::fs::read_dir("/sys/fs/cgroup/cirro")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("cirro-"))
        .collect();
    assert!(cgroups.is_empty(), "leftover VM cgroups: {cgroups:?}");
}

#[test]
fn run_serves_http_at_the_vm_address_and_stop_force_leaves_nothing() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm not present");
        return;
    }
    let Some(agent_bin) = install_test_agent() else {
        eprintln!(
            "skipping: `sudo -n {} --version` failed -- add the NOPASSWD sudoers rule \
             in this test's module doc first",
            test_agent_path().display()
        );
        return;
    };

    // Under $HOME, not /tmp: jailer mknods /dev/kvm in the jail, and /tmp
    // is usually a nodev tmpfs. Kept short so jail socket paths fit in a
    // sockaddr_un.
    let state_dir = home().join(format!(".cache/cirro-t{}", std::process::id()));
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    let rootfs = build_http_rootfs(&state_dir);
    let agent = Agent::start(&agent_bin, state_dir.clone());

    let run = agent
        .cirro()
        .args(["run", "--name", "web"])
        .arg(&rootfs)
        .args(["--", "/app/http_app"])
        .assert()
        .success();
    let address = String::from_utf8_lossy(&run.get_output().stdout)
        .trim()
        .to_string();
    assert!(
        address.starts_with(SUBNET_PREFIX),
        "`cirro run` should print a VM address in {SUBNET}, got {address:?}"
    );

    let response = wait_for_http(&address, HTTP_PORT);
    assert!(
        response.contains("hello from cirro"),
        "unexpected response from the VM: {response:?}"
    );

    let ps = agent.cirro().arg("ps").assert().success();
    let ps = String::from_utf8_lossy(&ps.get_output().stdout).into_owned();
    let web = ps
        .lines()
        .find(|l| l.split_whitespace().next() == Some("web"))
        .unwrap_or_else(|| panic!("`cirro ps` doesn't list web:\n{ps}"));
    assert!(web.contains(&address), "ps row lacks the VM address: {web}");
    assert!(web.contains("256M"), "ps row lacks the memory: {web}");

    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();

    let ps = agent.cirro().arg("ps").assert().success();
    let ps = String::from_utf8_lossy(&ps.get_output().stdout).into_owned();
    assert!(
        !ps.lines()
            .any(|l| l.split_whitespace().next() == Some("web")),
        "web is still listed after stop --force:\n{ps}"
    );
    assert_no_cirro_state(&state_dir);
}
