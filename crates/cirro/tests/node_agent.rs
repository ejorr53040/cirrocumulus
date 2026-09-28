//! M3's one seam: a throwaway Node agent under `sudo -n`, with its own
//! state dir, socket and Node subnet, driven only through the `cirro` CLI.
//! Every test ends by asserting the Node holds no Cirrocumulus state.
//!
//! Tests run in parallel, so each one gets its own Node subnet and only
//! checks for leftovers belonging to that subnet.
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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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
/// `sudo -n` will run it, once per test run. `None` means the rule is
/// missing.
fn test_agent() -> Option<&'static Path> {
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
struct Rootfs {
    /// guest-init as `/init`.
    guest_init: PathBuf,
    /// No `/init` at all, so the guest never starts guest-init.
    no_init: PathBuf,
    /// A `/init` that isn't guest-init and never takes a config.
    wrong_init: PathBuf,
}

fn rootfs() -> &'static Rootfs {
    static ROOTFS: OnceLock<Rootfs> = OnceLock::new();
    ROOTFS.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("node-agent-rootfs");
        let tree = dir.join("tree");
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["proc", "sys", "dev", "app"] {
            std::fs::create_dir_all(tree.join(sub)).expect("create rootfs tree");
        }
        for fixture in ["http_app", "ignore_term", "exit_later"] {
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
    file.set_len(32 * 1024 * 1024).expect("size rootfs image");
    let status = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(tree)
        .arg(image)
        .status()
        .expect("run mkfs.ext4");
    assert!(status.success(), "mkfs.ext4 failed");
    image.to_path_buf()
}

/// A throwaway Node agent on Node subnet `10.77.<octet>.0/24`. Dropping it
/// stops the agent and removes its state dir, so a failed assertion doesn't
/// strand anything.
struct Agent {
    sudo: Child,
    octet: u8,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Agent {
    /// Starts an agent whose socket belongs to the test user's group, or
    /// returns `None` (after saying why) when the test has to skip.
    fn start(octet: u8) -> Option<Agent> {
        Agent::start_with_socket_group(octet, &getgid().as_raw().to_string())
    }

    fn start_with_socket_group(octet: u8, socket_group: &str) -> Option<Agent> {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm not present");
            return None;
        }
        let Some(agent_bin) = test_agent() else {
            eprintln!(
                "skipping: `sudo -n {} --version` failed -- add the NOPASSWD sudoers rule \
                 in this test's module doc first",
                test_agent_path().display()
            );
            return None;
        };

        // Under $HOME, not /tmp: jailer mknods /dev/kvm in the jail, and /tmp
        // is usually a nodev tmpfs. Kept short so jail socket paths fit in a
        // sockaddr_un.
        let state_dir = home().join(format!(".cache/cirro-t{}-{octet}", std::process::id()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let socket = state_dir.join("agent.sock");
        let repo = repo_root();
        let sudo = std::process::Command::new("sudo")
            .arg("-n")
            .arg(agent_bin)
            .args(["node", "agent", "--state-dir"])
            .arg(&state_dir)
            .arg("--socket")
            .arg(&socket)
            .args(["--socket-group", socket_group])
            .args(["--subnet", &format!("10.77.{octet}.0/24")])
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
            octet,
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
        Some(agent)
    }

    fn cirro(&self) -> Command {
        let mut cmd = Command::cargo_bin("cirro").unwrap();
        cmd.env("CIRRO_SOCKET", &self.socket);
        cmd
    }

    /// `cirro run --name <name> <rootfs> -- <command...>`.
    fn run(&self, name: &str, rootfs: &Path, command: &[&str]) -> assert_cmd::assert::Assert {
        self.cirro()
            .args(["run", "--name", name])
            .arg(rootfs)
            .arg("--")
            .args(command)
            .assert()
    }

    /// `cirro ps`, or `cirro ps -a` when `all`.
    fn ps(&self, all: bool) -> String {
        let mut cmd = self.cirro();
        cmd.arg("ps");
        if all {
            cmd.arg("-a");
        }
        stdout(cmd.assert().success())
    }

    fn subnet_prefix(&self) -> String {
        format!("10.77.{}.", self.octet)
    }

    /// Everything a VM could leave on the Node, as seen from outside the
    /// agent. Host object names embed the Node subnet's third octet.
    fn assert_no_cirro_state(&self) {
        let id_prefix = format!("cirro-{:02x}", self.octet);
        let netns = stdout_of(&["ip", "netns", "list"]);
        assert!(
            !netns.lines().any(|l| l.starts_with(&id_prefix)),
            "leftover network namespaces:\n{netns}"
        );
        let links = stdout_of(&["ip", "-o", "link", "show"]);
        assert!(!links.contains(&id_prefix), "leftover links:\n{links}");
        let routes = stdout_of(&["ip", "route", "show"]);
        assert!(
            !routes.contains(&self.subnet_prefix()),
            "leftover routes into the Node subnet:\n{routes}"
        );
        let jails: Vec<_> = std::fs::read_dir(self.state_dir.join("jail/firecracker"))
            .into_iter()
            .flatten()
            .collect();
        assert!(jails.is_empty(), "leftover jail dirs: {jails:?}");
        let cgroups: Vec<_> = std::fs::read_dir("/sys/fs/cgroup/cirro")
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&id_prefix))
            .collect();
        assert!(cgroups.is_empty(), "leftover VM cgroups: {cgroups:?}");
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

fn stdout(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

/// The `ps` row for `name`, if listed.
fn row<'a>(ps: &'a str, name: &str) -> Option<&'a str> {
    ps.lines()
        .find(|l| l.split_whitespace().next() == Some(name))
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

#[test]
fn run_serves_http_at_the_vm_address_and_stop_force_leaves_nothing() {
    let Some(agent) = Agent::start(250) else {
        return;
    };

    let address = stdout(
        agent
            .run("web", &rootfs().guest_init, &["/app/http_app"])
            .success(),
    )
    .trim()
    .to_string();
    assert!(
        address.starts_with(&agent.subnet_prefix()),
        "`cirro run` should print a VM address in the Node subnet, got {address:?}"
    );

    let response = wait_for_http(&address, HTTP_PORT);
    assert!(
        response.contains("hello from cirro"),
        "unexpected response from the VM: {response:?}"
    );

    let ps = agent.ps(false);
    let web = row(&ps, "web").unwrap_or_else(|| panic!("`cirro ps` doesn't list web:\n{ps}"));
    assert!(web.contains(&address), "ps row lacks the VM address: {web}");
    assert!(web.contains("256M"), "ps row lacks the memory: {web}");

    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();

    let ps = agent.ps(false);
    assert!(
        row(&ps, "web").is_none(),
        "web is still listed after stop --force:\n{ps}"
    );
    agent.assert_no_cirro_state();
}

#[test]
fn invalid_and_duplicate_names_are_refused() {
    let Some(agent) = Agent::start(251) else {
        return;
    };

    let too_long = "a".repeat(33);
    for name in ["Web", "web_1", "web.1", "", too_long.as_str()] {
        let err = stderr(
            agent
                .run(name, &rootfs().guest_init, &["/app/http_app"])
                .failure(),
        );
        assert!(
            err.contains("name"),
            "refusing {name:?} should explain the name rules, got: {err}"
        );
    }
    let longest = "a".repeat(32);
    agent
        .run(&longest, &rootfs().guest_init, &["/app/http_app"])
        .success();

    agent
        .run("web-1", &rootfs().guest_init, &["/app/http_app"])
        .success();
    let err = stderr(
        agent
            .run("web-1", &rootfs().guest_init, &["/app/http_app"])
            .failure(),
    );
    assert!(
        err.contains("already exists"),
        "a duplicate name should be refused, got: {err}"
    );

    for name in [longest.as_str(), "web-1"] {
        agent
            .cirro()
            .args(["stop", "--force", name])
            .assert()
            .success();
    }
    agent.assert_no_cirro_state();
}

/// Polls `cirro logs <name>` until it contains `marker`.
fn wait_for_log(agent: &Agent, name: &str, marker: &str) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let logs = stdout(agent.cirro().args(["logs", name]).assert().success());
        if logs.contains(marker) {
            return logs;
        }
        assert!(
            Instant::now() < deadline,
            "`cirro logs {name}` never showed {marker:?}; got:\n{logs}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn graceful_stop_leaves_an_ended_vm_whose_logs_last_until_rm() {
    let Some(agent) = Agent::start(252) else {
        return;
    };

    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");
    let err = stderr(agent.cirro().args(["rm", "web"]).assert().failure());
    assert!(
        err.contains("running"),
        "rm should refuse a running VM, got: {err}"
    );

    agent.cirro().args(["stop", "web"]).assert().success();
    assert!(
        row(&agent.ps(false), "web").is_none(),
        "plain ps should not list an Ended VM"
    );
    let ps = agent.ps(true);
    let ended = row(&ps, "web").unwrap_or_else(|| panic!("ps -a doesn't list web:\n{ps}"));
    assert!(ended.contains("graceful"), "end reason missing: {ended}");
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");

    // A reuse that fails to start replaces nothing.
    agent
        .run("web", &rootfs().no_init, &["/app/http_app"])
        .failure();
    let ps = agent.ps(true);
    let ended = row(&ps, "web").unwrap_or_else(|| panic!("failed reuse lost web:\n{ps}"));
    assert!(
        ended.contains("graceful"),
        "failed reuse changed web: {ended}"
    );
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");

    // Reusing the name replaces the Ended VM's record.
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    let ps = agent.ps(true);
    let rows: Vec<_> = ps
        .lines()
        .filter(|l| l.split_whitespace().next() == Some("web"))
        .collect();
    assert_eq!(rows.len(), 1, "one record per name:\n{ps}");
    assert!(
        !rows[0].contains("graceful"),
        "old record kept: {}",
        rows[0]
    );

    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    let ps = agent.ps(true);
    let ended = row(&ps, "web").unwrap_or_else(|| panic!("ps -a doesn't list web:\n{ps}"));
    assert!(ended.contains("forced"), "end reason missing: {ended}");

    agent.cirro().args(["rm", "web"]).assert().success();
    assert!(row(&agent.ps(true), "web").is_none(), "rm left the record");
    agent.cirro().args(["logs", "web"]).assert().failure();
    agent.assert_no_cirro_state();
}

#[test]
fn graceful_stop_forces_a_vm_that_ignores_sigterm_after_its_timeout() {
    let Some(agent) = Agent::start(253) else {
        return;
    };

    agent
        .run("stubborn", &rootfs().guest_init, &["/app/ignore_term"])
        .success();
    wait_for_log(&agent, "stubborn", "IGNORE_TERM_STARTED");

    let started = Instant::now();
    agent
        .cirro()
        .args(["stop", "--timeout", "2", "stubborn"])
        .assert()
        .success();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(8),
        "stop --timeout 2 should give up after about 2s, took {took:?}"
    );
    let ps = agent.ps(true);
    let ended = row(&ps, "stubborn").unwrap_or_else(|| panic!("ps -a lacks stubborn:\n{ps}"));
    assert!(ended.contains("forced"), "end reason missing: {ended}");
    // guest-init did pass the stop on, and the command ignored it.
    wait_for_log(&agent, "stubborn", "GUEST_INIT_FORWARDING_SIGTERM");
    agent.assert_no_cirro_state();
}

#[test]
fn run_reports_why_a_vm_failed_to_start_and_leaves_nothing() {
    let Some(agent) = Agent::start(254) else {
        return;
    };

    // No guest-init: the guest kernel finds no init and panics.
    let started = Instant::now();
    let err = stderr(
        agent
            .run("no-init", &rootfs().no_init, &["/app/http_app"])
            .failure(),
    );
    assert!(
        err.contains("crashed") && err.contains("Kernel panic"),
        "run should say the VM crashed and show the console tail, got:\n{err}"
    );
    assert!(
        started.elapsed() < TIMEOUT,
        "a guest that panics should fail run at once, not at the config timeout"
    );

    // A command that doesn't exist: guest-init can't exec it and the VM ends.
    let err = stderr(
        agent
            .run("no-cmd", &rootfs().guest_init, &["/app/does-not-exist"])
            .failure(),
    );
    assert!(
        err.contains("exited on its own") && err.contains("execv"),
        "run should say the VM exited and show why, got:\n{err}"
    );

    let ps = agent.ps(true);
    for name in ["no-init", "no-cmd"] {
        assert!(
            row(&ps, name).is_none(),
            "a failed run left a record:\n{ps}"
        );
    }
    agent.assert_no_cirro_state();
}

#[test]
fn a_user_outside_the_socket_group_gets_a_clear_permission_error() {
    // The test user isn't in group 0 (root), so the socket is closed to it.
    let Some(agent) = Agent::start_with_socket_group(249, "0") else {
        return;
    };
    let err = stderr(agent.cirro().arg("ps").assert().failure());
    assert!(
        err.contains("permission denied") && err.contains("group"),
        "expected a permission error naming the group, got: {err}"
    );
    agent.assert_no_cirro_state();
}

#[test]
fn logs_follow_streams_a_running_vm_until_it_ends() {
    let Some(agent) = Agent::start(248) else {
        return;
    };
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();

    let mut follow = std::process::Command::new(env!("CARGO_BIN_EXE_cirro"))
        .env("CIRRO_SOCKET", &agent.socket)
        .args(["logs", "-f", "web"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("start cirro logs -f");
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");
    agent.cirro().args(["stop", "web"]).assert().success();

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = follow.try_wait().expect("poll cirro logs -f") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = follow.kill();
            panic!("`cirro logs -f` kept running after the VM ended");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "`cirro logs -f` failed: {status}");
    let mut output = String::new();
    follow
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    // Output from before and after the stop: it followed, not just dumped.
    for marker in ["HTTP_FIXTURE_LISTENING", "GUEST_INIT_FORWARDING_SIGTERM"] {
        assert!(
            output.contains(marker),
            "logs -f missed {marker}:\n{output}"
        );
    }
    agent.assert_no_cirro_state();
}

#[test]
fn a_vm_whose_command_exits_on_its_own_is_recorded_as_exited() {
    let Some(agent) = Agent::start(247) else {
        return;
    };
    agent
        .run("brief", &rootfs().guest_init, &["/app/exit_later"])
        .success();

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let ps = agent.ps(true);
        let brief = row(&ps, "brief").unwrap_or_else(|| panic!("ps -a lacks brief:\n{ps}"));
        if !brief.contains("running") {
            assert!(brief.contains("exited"), "wrong end reason: {brief}");
            break;
        }
        assert!(Instant::now() < deadline, "brief never ended: {brief}");
        std::thread::sleep(Duration::from_millis(200));
    }
    wait_for_log(&agent, "brief", "EXIT_LATER_DONE");
    agent.assert_no_cirro_state();
}

#[test]
fn run_gives_up_on_a_vm_that_never_takes_its_config() {
    let Some(agent) = Agent::start(246) else {
        return;
    };
    // `/init` runs but isn't guest-init, so no config is ever taken.
    let err = stderr(
        agent
            .run("deaf", &rootfs().wrong_init, &["/app/http_app"])
            .failure(),
    );
    assert!(
        err.contains("never accepted its config") && err.contains("IGNORE_TERM_STARTED"),
        "run should report the config timeout with the console tail, got:\n{err}"
    );
    assert!(
        row(&agent.ps(true), "deaf").is_none(),
        "a failed run left a record"
    );
    agent.assert_no_cirro_state();
}
