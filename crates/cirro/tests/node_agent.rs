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
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const HTTP_PORT: u16 = 8080;
const TIMEOUT: Duration = Duration::from_secs(10);

/// The Node subnet octets of the agents currently running in this process.
static LIVE_OCTETS: std::sync::Mutex<std::collections::BTreeSet<u8>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

mod common;
use common::{home, latest_kernel, repo_root, test_agent, test_agent_path};

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
        for fixture in [
            "http_app",
            "ignore_term",
            "exit_later",
            "probe",
            "whoami",
            "spin",
            "counter",
            "dialer",
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

/// A throwaway Node agent on Node subnet `10.77.<octet>.0/24`. It can be
/// stopped and restarted on the same state dir, like the real one. Dropping
/// it ends every VM the test left running, stops the agent and removes its
/// state dir, so a failed assertion doesn't strand anything.
struct Agent {
    /// The `sudo` running the agent; `None` while the agent is stopped.
    sudo: Option<Child>,
    octet: u8,
    socket_group: String,
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
        if test_agent().is_none() {
            eprintln!(
                "skipping: `sudo -n {} --version` failed -- add the NOPASSWD sudoers rule \
                 in this test's module doc first",
                test_agent_path().display()
            );
            return None;
        }

        // Two live agents on one subnet sweep each other's VMs, which shows
        // up as unrelated failures elsewhere in the suite.
        let free = LIVE_OCTETS.lock().unwrap().insert(octet);
        assert!(
            free,
            "octet {octet} is already used by another running test; give this test its own"
        );

        // Under $HOME, not /tmp: jailer mknods /dev/kvm in the jail, and /tmp
        // is usually a nodev tmpfs. Kept short so jail socket paths fit in a
        // sockaddr_un.
        let state_dir = home().join(format!(".cache/cirro-t{}-{octet}", std::process::id()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let socket = state_dir.join("agent.sock");
        let mut agent = Agent {
            sudo: None,
            octet,
            socket_group: socket_group.to_string(),
            state_dir,
            socket,
        };
        agent.spawn().expect("start the Node agent");
        Some(agent)
    }

    /// Starts the agent process on this agent's state dir and waits for its
    /// socket, which the agent only opens once it is ready for requests.
    fn spawn(&mut self) -> Result<(), String> {
        // A crashed agent leaves its socket file behind, and it would look
        // like the new agent's.
        let _ = std::fs::remove_file(&self.socket);
        let sudo = self
            .agent_command(self.octet)
            .spawn()
            .map_err(|e| format!("spawn sudo: {e}"))?;
        self.sudo = Some(sudo);
        let deadline = Instant::now() + TIMEOUT;
        while !self.socket.exists() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "the Node agent never created its socket at {}",
                    self.socket.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// The command that starts an agent on this state dir, for Node subnet
    /// `10.77.<octet>.0/24`.
    fn agent_command(&self, octet: u8) -> std::process::Command {
        let agent_bin = test_agent().expect("the harness checked for the test agent at start");
        let repo = repo_root();
        let mut cmd = std::process::Command::new("sudo");
        cmd.arg("-n")
            .arg(agent_bin)
            .args(["node", "agent", "--state-dir"])
            .arg(&self.state_dir)
            .arg("--socket")
            .arg(&self.socket)
            .args(["--socket-group", &self.socket_group])
            .args(["--subnet", &format!("10.77.{octet}.0/24")])
            .arg("--firecracker")
            .arg(repo.join("firecracker"))
            .arg("--jailer")
            .arg(repo.join("jailer"))
            .arg("--kernel")
            .arg(latest_kernel());
        cmd
    }

    /// Stops the agent the way systemd does (SIGTERM, which `sudo` relays)
    /// and waits for it to exit. VMs are not the agent's to stop.
    fn stop_agent(&mut self) {
        if let Some(mut sudo) = self.sudo.take() {
            let _ = kill(Pid::from_raw(sudo.id() as i32), Signal::SIGTERM);
            let _ = sudo.wait();
        }
    }

    /// Kills the agent the way a crash does: SIGUSR1, which `sudo` relays and
    /// the agent doesn't handle, ends it at once without its shutdown path.
    fn crash_agent(&mut self) {
        if let Some(mut sudo) = self.sudo.take() {
            let _ = kill(Pid::from_raw(sudo.id() as i32), Signal::SIGUSR1);
            let _ = sudo.wait();
        }
    }

    /// Stops the agent if it is running, then starts it on the same state dir.
    fn restart(&mut self) {
        self.stop_agent();
        self.spawn().expect("restart the Node agent");
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

impl Agent {
    /// Stops the agent and checks that no snapshot is left in its state
    /// dir. The parked dir is root-only, so it can't be listed from here,
    /// but the agent removes it on the way out only when it's empty.
    fn assert_no_snapshots(&mut self) {
        self.stop_agent();
        let parked = self.state_dir.join("parked");
        assert!(
            !parked.exists(),
            "a snapshot is left in {} after the agent stopped",
            parked.display()
        );
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // VMs outlive the agent, so a test that failed part-way has to have
        // its VMs ended, and their records and logs removed, through an agent.
        if self.sudo.is_none() {
            let _ = self.spawn();
        }
        if self.sudo.is_some() {
            let listing = self.cirro().args(["ps", "-a"]).output();
            let listing = listing.map_or(String::new(), |o| {
                String::from_utf8_lossy(&o.stdout).into_owned()
            });
            for name in listing
                .lines()
                .skip(1)
                .filter_map(|l| l.split_whitespace().next())
            {
                let _ = self.cirro().args(["stop", "--force", name]).output();
                let _ = self.cirro().args(["rm", name]).output();
            }
        }
        self.stop_agent();
        let _ = std::fs::remove_dir_all(&self.state_dir);
        LIVE_OCTETS.lock().unwrap().remove(&self.octet);
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

/// A raw `POST {path}` straight over the agent's Unix socket, bypassing
/// `cirro`'s own `clap` argument validation entirely -- the socket, not the
/// CLI, is the actual trust boundary (ADR 0002), so some findings can only
/// be proven at this seam (2026-09-28 security audit, #2: a client that
/// isn't the `cirro` CLI can send a `RunRequest` clap would have rejected).
fn post_raw(socket: &Path, path: &str, json_body: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    write!(
        stream,
        "POST {path} HTTP/1.0\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {json_body}",
        json_body.len()
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
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
fn a_vm_outlives_a_restarted_agent_and_is_still_the_agents_to_stop() {
    let Some(mut agent) = Agent::start(244) else {
        return;
    };
    let address = stdout(
        agent
            .run("web", &rootfs().guest_init, &["/app/http_app"])
            .success(),
    )
    .trim()
    .to_string();
    wait_for_http(&address, HTTP_PORT);

    agent.restart();

    let ps = agent.ps(false);
    let web = row(&ps, "web")
        .unwrap_or_else(|| panic!("web isn't listed after the agent restarted:\n{ps}"));
    assert!(
        web.contains(&address),
        "the VM address changed across the restart: {web}"
    );
    let response = wait_for_http(&address, HTTP_PORT);
    assert!(
        response.contains("hello from cirro"),
        "the VM stopped answering after the agent restarted: {response:?}"
    );
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");

    // The re-adopted VM is stopped like any other, through its API socket.
    agent.cirro().args(["stop", "web"]).assert().success();
    let ps = agent.ps(true);
    let ended = row(&ps, "web").unwrap_or_else(|| panic!("ps -a doesn't list web:\n{ps}"));
    assert!(ended.contains("graceful"), "end reason missing: {ended}");
    agent.assert_no_cirro_state();
}

/// Waits until nothing runs in the cgroup of the VM at `10.77.<octet>.<host>`,
/// which is when its VMM has exited. The cgroup itself stays until an agent
/// removes it.
fn wait_for_vmm_exit(octet: u8, host: u8) {
    let procs = format!("/sys/fs/cgroup/cirro/cirro-{octet:02x}{host:02x}/cgroup.procs");
    let deadline = Instant::now() + TIMEOUT;
    while !std::fs::read_to_string(&procs).is_ok_and(|p| p.trim().is_empty()) {
        assert!(Instant::now() < deadline, "the VMM in {procs} never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_vm_that_died_while_the_agent_was_down_is_ended_and_its_host_state_removed() {
    let Some(mut agent) = Agent::start(243) else {
        return;
    };
    agent
        .run("brief", &rootfs().guest_init, &["/app/exit_later"])
        .success();
    wait_for_log(&agent, "brief", "EXIT_LATER_STARTED");

    // Nobody is watching when the VM's command exits and its VMM with it.
    agent.stop_agent();
    wait_for_vmm_exit(243, 2);
    agent.restart();

    assert!(
        row(&agent.ps(false), "brief").is_none(),
        "a VM that died isn't running"
    );
    let ps = agent.ps(true);
    let brief = row(&ps, "brief").unwrap_or_else(|| panic!("ps -a lacks brief:\n{ps}"));
    assert!(
        brief.contains("died while the agent was down"),
        "wrong end reason: {brief}"
    );
    wait_for_log(&agent, "brief", "EXIT_LATER_DONE");
    agent.assert_no_cirro_state();
}

#[test]
fn an_agent_refuses_a_state_dir_that_belongs_to_another_node_subnet() {
    let Some(mut agent) = Agent::start(238) else {
        return;
    };
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    agent.stop_agent();

    let output = run_until_it_exits(agent.agent_command(237))
        .expect("an agent with the wrong subnet should refuse to start, not run");
    assert!(!output.status.success(), "it should fail: {output:?}");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("10.77.238.0/24") && err.contains("10.77.237.0/24"),
        "the refusal should name both subnets, got: {err}"
    );

    // The refusal changed nothing: the right subnet still gets its VM back.
    agent.restart();
    let ps = agent.ps(false);
    assert!(row(&ps, "web").is_some(), "web was lost:\n{ps}");
    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

#[test]
fn an_agent_refuses_to_start_on_a_running_record_it_cannot_make_sense_of() {
    let Some(mut agent) = Agent::start(236) else {
        return;
    };
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    agent.stop_agent();

    // Damage the record of the running VM: the state dir is ours, so a
    // replacement database can be moved over the agent's.
    let db = agent.state_dir.join("state.db");
    let original = agent.state_dir.join("state.db.original");
    std::fs::copy(&db, &original).expect("keep the real database");
    let damaged = agent.state_dir.join("state.db.damaged");
    std::fs::copy(&db, &damaged).expect("copy the database");
    rusqlite::Connection::open(&damaged)
        .and_then(|conn| conn.execute("UPDATE vms SET pid = NULL, pid_start = NULL", []))
        .expect("damage the record");
    std::fs::rename(&damaged, &db).expect("install the damaged database");

    let output = run_until_it_exits(agent.agent_command(236))
        .expect("an agent that can't trust a record should refuse to start, not guess");
    assert!(!output.status.success(), "it should fail: {output:?}");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("web"),
        "the refusal should name the VM, got: {err}"
    );

    // Refusing left the VM alone, so with the record intact it is taken back.
    std::fs::rename(&original, &db).expect("restore the real database");
    agent.restart();
    let ps = agent.ps(false);
    assert!(row(&ps, "web").is_some(), "web was lost:\n{ps}");
    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

#[test]
fn an_agent_refuses_to_start_while_another_owns_its_socket() {
    let Some(agent) = Agent::start(249) else {
        return;
    };
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();

    // The first agent is still alive: a second attempt on the same socket
    // must refuse, not steal it out from under the first.
    let output = run_until_it_exits(agent.agent_command(agent.octet))
        .expect("an agent whose socket is already live should refuse to start, not run");
    assert!(!output.status.success(), "it should fail: {output:?}");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains(&agent.socket.display().to_string()),
        "the refusal should name the socket, got: {err}"
    );

    // Refusing touched nothing: the first agent still owns its VM.
    let ps = agent.ps(false);
    assert!(row(&ps, "web").is_some(), "web was lost:\n{ps}");
    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

#[test]
fn a_vm_taken_back_after_a_restart_can_be_force_stopped() {
    let Some(mut agent) = Agent::start(239) else {
        return;
    };
    // Ignores SIGTERM, so only a kill of the VMM itself can end it.
    agent
        .run("stubborn", &rootfs().guest_init, &["/app/ignore_term"])
        .success();
    agent.restart();

    agent
        .cirro()
        .args(["stop", "--force", "stubborn"])
        .assert()
        .success();
    let ps = agent.ps(true);
    let ended = row(&ps, "stubborn").unwrap_or_else(|| panic!("ps -a lacks stubborn:\n{ps}"));
    assert!(ended.contains("forced"), "end reason missing: {ended}");
    agent.assert_no_cirro_state();
}

#[test]
fn an_ended_vm_its_logs_and_its_removal_survive_agent_restarts() {
    let Some(mut agent) = Agent::start(242) else {
        return;
    };
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");
    agent.cirro().args(["stop", "web"]).assert().success();

    agent.restart();

    assert!(
        row(&agent.ps(false), "web").is_none(),
        "an Ended VM isn't running"
    );
    let ps = agent.ps(true);
    let ended = row(&ps, "web").unwrap_or_else(|| panic!("ps -a lost web:\n{ps}"));
    assert!(
        ended.contains("graceful"),
        "the end reason changed across the restart: {ended}"
    );
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");

    agent.cirro().args(["rm", "web"]).assert().success();
    agent.restart();
    assert!(
        row(&agent.ps(true), "web").is_none(),
        "a removed VM came back after the restart"
    );
    agent.cirro().args(["logs", "web"]).assert().failure();
    agent.assert_no_cirro_state();
}

#[test]
fn a_failed_reuse_of_a_name_after_a_restart_leaves_the_ended_vms_log() {
    let Some(mut agent) = Agent::start(241) else {
        return;
    };
    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");
    agent.cirro().args(["stop", "web"]).assert().success();
    agent.restart();

    // A directory exists, so the CLI passes it on, but the agent can't copy it.
    agent
        .run("web", Path::new("/tmp"), &["/app/http_app"])
        .failure();

    let ps = agent.ps(true);
    let ended = row(&ps, "web").unwrap_or_else(|| panic!("the failed reuse lost web:\n{ps}"));
    assert!(ended.contains("graceful"), "web changed: {ended}");
    wait_for_log(&agent, "web", "HTTP_FIXTURE_LISTENING");
    agent.assert_no_cirro_state();
}

/// Runs an agent that is expected to refuse to start, and returns how it
/// ended: `None` if it was still running after `TIMEOUT`, when it is stopped.
fn run_until_it_exits(mut agent: std::process::Command) -> Option<std::process::Output> {
    let mut child = agent
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the agent");
    let deadline = Instant::now() + TIMEOUT;
    while child.try_wait().expect("poll the agent").is_none() {
        if Instant::now() >= deadline {
            // sudo relays SIGTERM to the agent it started.
            let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Some(
        child
            .wait_with_output()
            .expect("collect the agent's output"),
    )
}

/// Whatever host state the VM at `10.77.<octet>.<host>` has on the Node.
fn host_state_of(octet: u8, host: u8) -> Vec<String> {
    let id = format!("cirro-{octet:02x}{host:02x}");
    let exists = |path: String| Path::new(&path).exists();
    let mut found = Vec::new();
    for path in [
        format!("/run/netns/{id}"),
        format!("/sys/class/net/{id}"),
        format!("/sys/fs/cgroup/cirro/{id}"),
    ] {
        if exists(path.clone()) {
            found.push(path);
        }
    }
    found
}

#[test]
fn a_crashed_agent_leaves_vms_running_and_a_restart_removes_what_a_start_left_behind() {
    let Some(mut agent) = Agent::start(240) else {
        return;
    };
    let address = stdout(
        agent
            .run("web", &rootfs().guest_init, &["/app/http_app"])
            .success(),
    )
    .trim()
    .to_string();
    wait_for_http(&address, HTTP_PORT);

    // A second `run` that never finishes starting: its guest never takes its
    // config. Wait until its Firecracker is up, so the crash leaves a running
    // process and host state that no record names.
    let mut stuck = agent.cirro();
    stuck.args(["run", "--name", "stuck"]);
    stuck.arg(&rootfs().no_init).args(["--", "/app/http_app"]);
    let stuck = std::thread::spawn(move || stuck.output());
    let procs = "/sys/fs/cgroup/cirro/cirro-f003/cgroup.procs";
    let deadline = Instant::now() + TIMEOUT;
    while std::fs::read_to_string(procs).is_ok_and(|p| p.trim().is_empty())
        || !Path::new(procs).exists()
    {
        assert!(Instant::now() < deadline, "stuck never got a Firecracker");
        std::thread::sleep(Duration::from_millis(50));
    }

    agent.crash_agent();
    assert!(
        !stuck.join().unwrap().unwrap().status.success(),
        "the client of a crashed agent should fail"
    );
    agent.restart();

    let ps = agent.ps(false);
    let web = row(&ps, "web")
        .unwrap_or_else(|| panic!("web isn't listed after the agent crashed:\n{ps}"));
    assert!(web.contains(&address), "web's address changed: {web}");
    assert!(
        row(&ps, "stuck").is_none(),
        "a VM that never finished starting was adopted:\n{ps}"
    );
    assert_eq!(
        host_state_of(240, 3),
        Vec::<String>::new(),
        "the restart left what the unfinished start made"
    );
    let logs: Vec<String> = std::fs::read_dir(agent.state_dir.join("logs"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    assert!(
        logs.iter().all(|l| l.starts_with("web.")),
        "the restart left a console log no record names: {logs:?}"
    );
    let response = wait_for_http(&address, HTTP_PORT);
    assert!(
        response.contains("hello from cirro"),
        "web stopped answering after the crash: {response:?}"
    );

    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
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

/// A rootfs the caller can't read (#14): the agent runs as root, but must
/// still be refused the file, since a `cirro` group member is only meant
/// to control VMs, not read arbitrary root-readable files on the host.
#[test]
fn run_refuses_a_rootfs_the_caller_cant_read_and_leaves_nothing() {
    let Some(agent) = Agent::start(233) else {
        return;
    };

    let denied = Path::new(env!("CARGO_TARGET_TMPDIR")).join("denied-rootfs.ext4");
    std::fs::copy(&rootfs().guest_init, &denied).expect("copy a rootfs to make an unreadable one");
    // Resolved the same way `cirro run` resolves it, so it matches
    // however the refusal names the path.
    let denied_canonical = std::fs::canonicalize(&denied).expect("canonicalize the rootfs");
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000))
        .expect("chmod 000 the rootfs");
    let err = stderr(agent.run("denied", &denied, &["/app/http_app"]).failure());
    // Restore read access so the fixture can be cleaned up (or reused: the
    // rootfs dir is only built once per test run).
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o644))
        .expect("restore the rootfs's permissions");

    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected a permission error, got: {err}"
    );
    // Names the caller's own rootfs path, not e.g. a Firecracker error
    // about the copy already sitting in the jail: this must be refused
    // before the file is ever copied as root, not just fail incidentally
    // once Firecracker itself can't open a copy that inherited 000 mode
    // bits (which `std::fs::copy` preserves from the source).
    assert!(
        err.contains(&denied_canonical.display().to_string()),
        "expected the refusal to name the rootfs path {}, got: {err}",
        denied_canonical.display()
    );
    let ps = agent.ps(true);
    assert!(
        row(&ps, "denied").is_none(),
        "a refused run left a record:\n{ps}"
    );
    agent.assert_no_cirro_state();
}

/// `/dev/zero` as a rootfs (#14): the agent would otherwise copy forever
/// into the state dir's filesystem until it fills.
#[test]
fn run_refuses_a_rootfs_thats_not_a_regular_file_and_leaves_nothing() {
    let Some(agent) = Agent::start(232) else {
        return;
    };

    let started = Instant::now();
    let err = stderr(
        agent
            .run("not-regular", Path::new("/dev/zero"), &["/app/http_app"])
            .failure(),
    );
    assert!(
        err.contains("not a regular file"),
        "expected a 'not a regular file' error, got: {err}"
    );
    assert!(
        started.elapsed() < TIMEOUT,
        "a rootfs that isn't a regular file should fail at once, not at the boot timeout"
    );
    let ps = agent.ps(true);
    assert!(
        row(&ps, "not-regular").is_none(),
        "a refused run left a record:\n{ps}"
    );
    agent.assert_no_cirro_state();
}

/// 2026-09-28 security audit, #2: `clap` bounds `--mem`/`--vcpus` in the
/// CLI, but the CLI isn't the trust boundary -- ADR 0002 says the socket
/// is. A raw client that skips `cirro` entirely must still be refused by
/// the agent itself.
#[test]
fn run_rejects_resource_requests_outside_the_agents_own_bounds() {
    let Some(agent) = Agent::start(235) else {
        return;
    };
    let rootfs = rootfs().guest_init.display();

    for (field, mem_mib, vcpus) in [
        ("vcpus", "256", "0"),
        ("vcpus", "256", "255"),
        ("mem_mib", "0", "1"),
        ("mem_mib", "4294967295", "1"),
    ] {
        let body = format!(
            r#"{{"name":"oob","rootfs":"{rootfs}","mem_mib":{mem_mib},"vcpus":{vcpus},"command":["/app/http_app"]}}"#
        );
        let response = post_raw(&agent.socket, "/vms", &body).expect("send a raw /vms request");
        assert!(
            response.contains(" 400 "),
            "expected a 400 for out-of-range {field}, got: {response}"
        );
        assert!(
            response.contains(field),
            "expected the error to name {field}, got: {response}"
        );
    }

    let ps = agent.ps(true);
    assert!(
        row(&ps, "oob").is_none(),
        "a rejected raw request left a record:\n{ps}"
    );
    agent.assert_no_cirro_state();
}

/// 2026-09-28 security audit, #7: `run_vm` validates `name`, but `stop`,
/// `logs` and `remove` didn't -- inconsistent, and each would otherwise
/// echo an unvalidated client-supplied name back into its own errors.
#[test]
fn stop_logs_and_rm_reject_invalid_names_up_front() {
    let Some(agent) = Agent::start(234) else {
        return;
    };
    for args in [
        vec!["stop", "BADNAME"],
        vec!["logs", "BADNAME"],
        vec!["rm", "BADNAME"],
    ] {
        let err = stderr(agent.cirro().args(&args).assert().failure());
        assert!(
            err.contains("invalid VM name"),
            "expected {args:?} to reject the name up front like `run` does, got: {err}"
        );
    }
    agent.assert_no_cirro_state();
}

/// Console logs hold whatever the VM's command printed, so they should be
/// readable to the `cirro` group but not to everyone (#14).
#[test]
fn console_logs_are_not_world_readable() {
    let Some(agent) = Agent::start(231) else {
        return;
    };

    agent
        .run("web", &rootfs().guest_init, &["/app/http_app"])
        .success();
    let logs_dir = agent.state_dir.join("logs");
    let log = std::fs::read_dir(&logs_dir)
        .expect("read the agent's logs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with("web."))
        .unwrap_or_else(|| panic!("no console log for web under {}", logs_dir.display()))
        .path();
    let mode = std::fs::metadata(&log)
        .expect("stat the console log")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode,
        0o640,
        "console log {} should be 0640, was {mode:o}",
        log.display()
    );

    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

#[test]
fn a_user_outside_the_socket_group_gets_a_clear_permission_error() {
    // The test user isn't in group 0 (root), so the socket is closed to it.
    let Some(agent) = Agent::start_with_socket_group(230, "0") else {
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

/// Whether this host itself can reach `addr` (a TCP answer, even a
/// refusal, counts), so a test doesn't blame the VM for the network.
fn host_reaches(addr: SocketAddr) -> bool {
    match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::ConnectionRefused,
    }
}

/// The Node's default gateway: the nearest thing on its LAN that answers.
fn default_gateway() -> Option<std::net::Ipv4Addr> {
    stdout_of(&["ip", "-o", "-4", "route", "show", "default"])
        .split_whitespace()
        .skip_while(|w| *w != "via")
        .nth(1)?
        .parse()
        .ok()
}

/// Runs the probe fixture in a VM against `targets` and returns each
/// target's result word (`OPEN`, `REFUSED` or `FAILED`).
fn probe_from_vm(agent: &Agent, name: &str, targets: &[String]) -> Vec<(String, String)> {
    let mut command = vec!["/app/probe"];
    command.extend(targets.iter().map(String::as_str));
    agent.run(name, &rootfs().guest_init, &command).success();
    // Each blocked target takes the probe's full 3 s timeout.
    let deadline = Instant::now() + TIMEOUT + Duration::from_secs(4 * targets.len() as u64);
    let logs = loop {
        let logs = stdout(agent.cirro().args(["logs", name]).assert().success());
        if logs.contains("PROBE_DONE") {
            break logs;
        }
        assert!(Instant::now() < deadline, "probe never finished:\n{logs}");
        std::thread::sleep(Duration::from_millis(200));
    };
    agent
        .cirro()
        .args(["stop", "--force", name])
        .assert()
        .success();
    targets
        .iter()
        .map(|t| {
            let word = logs
                .lines()
                .find_map(|l| l.strip_prefix(&format!("PROBE {t} ")))
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_else(|| panic!("no probe result for {t}:\n{logs}"));
            (t.clone(), word.to_string())
        })
        .collect()
}

#[test]
fn vms_reach_the_internet_but_not_smtp_each_other_or_the_lan() {
    let internet: SocketAddr = "1.1.1.1:443".parse().unwrap();
    if !host_reaches(internet) {
        eprintln!("skipping: this host can't reach {internet}, so VM egress can't be judged");
        return;
    }
    // A real SMTP server, and another of its ports that this host can reach
    // (networks filter these differently), to show the drop is about port
    // 25, not the server.
    let smtp = ("smtp.gmail.com", 25)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.find(SocketAddr::is_ipv4))
        .filter(|addr| host_reaches(*addr));
    if smtp.is_none() {
        eprintln!("note: this host can't reach smtp.gmail.com:25 itself; skipping the SMTP check");
    }
    let smtp_other_port = smtp.and_then(|smtp| {
        [465, 587]
            .into_iter()
            .map(|port| SocketAddr::from((smtp.ip(), port)))
            .find(|addr| host_reaches(*addr))
    });
    let gateway = default_gateway()
        .map(|gw| SocketAddr::from((gw, 80)))
        .filter(|addr| host_reaches(*addr));
    if gateway.is_none() {
        eprintln!("note: the default gateway doesn't answer on port 80; skipping the LAN check");
    }

    // The policy is applied at agent startup and must survive a restart.
    drop(Agent::start(245));
    let Some(agent) = Agent::start(245) else {
        return;
    };

    let target = stdout(
        agent
            .run("target", &rootfs().guest_init, &["/app/http_app"])
            .success(),
    )
    .trim()
    .to_string();
    let other_vm = format!("{target}:{HTTP_PORT}");

    let mut targets = vec![internet.to_string(), other_vm.clone()];
    targets.extend(smtp.iter().chain(&smtp_other_port).map(ToString::to_string));
    if let Some(gateway) = gateway {
        targets.push(gateway.to_string());
    }
    let results = probe_from_vm(&agent, "prober", &targets);
    let result = |t: &str| results.iter().find(|(k, _)| k == t).unwrap().1.clone();

    assert_eq!(
        result(&internet.to_string()),
        "OPEN",
        "the internet: {results:?}"
    );
    assert_eq!(result(&other_vm), "FAILED", "another VM: {results:?}");
    if let Some(smtp) = smtp {
        assert_eq!(result(&smtp.to_string()), "FAILED", "SMTP: {results:?}");
    }
    if let Some(other) = smtp_other_port {
        assert_eq!(
            result(&other.to_string()),
            "OPEN",
            "the SMTP server's other port: {results:?}"
        );
    }
    if let Some(gateway) = gateway {
        assert_eq!(
            result(&gateway.to_string()),
            "FAILED",
            "the LAN: {results:?}"
        );
    }

    // The Node can still reach the VM.
    let response = wait_for_http(&target, HTTP_PORT);
    assert!(
        response.contains("hello from cirro"),
        "Node lost the VM: {response}"
    );
    agent
        .cirro()
        .args(["stop", "--force", "target"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

#[test]
fn run_sets_the_commands_env_workdir_and_user_and_finds_it_on_path() {
    let Some(agent) = Agent::start(229) else {
        return;
    };
    agent
        .cirro()
        .args([
            "run",
            "--name",
            "who",
            "--workdir",
            "/app",
            "--user",
            "1000:1001",
        ])
        .args(["--env", "PATH=/bin:/app", "--env", "GREETING=hello there"])
        .arg(&rootfs().guest_init)
        .args(["--", "whoami"])
        .assert()
        .success();

    let logs = wait_for_log(&agent, "who", "WHOAMI_DONE");
    for line in [
        "WHOAMI uid=1000 gid=1001",
        "WHOAMI cwd=/app",
        "WHOAMI env PATH=/bin:/app",
        "WHOAMI env GREETING=hello there",
        // Added when the request doesn't set it, as Docker does.
        "WHOAMI env HOME=/",
    ] {
        assert!(logs.contains(line), "missing {line:?} in:\n{logs}");
    }
    agent
        .cirro()
        .args(["stop", "--force", "who"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

/// guest-init is PID 1: a config it can't use would crash the guest, so the
/// agent refuses such a request itself, whatever client sent it.
#[test]
fn run_rejects_an_env_workdir_or_command_the_guest_cant_use() {
    let Some(agent) = Agent::start(228) else {
        return;
    };
    let rootfs = rootfs().guest_init.display();
    for (field, extra, command) in [
        ("env", r#""env":["NO_EQUALS_SIGN"]"#, r#"["/app/whoami"]"#),
        ("env", r#""env":["=value"]"#, r#"["/app/whoami"]"#),
        ("env", r#""env":["A=nul\u0000byte"]"#, r#"["/app/whoami"]"#),
        ("workdir", r#""workdir":"relative""#, r#"["/app/whoami"]"#),
        ("command", r#""env":[]"#, r#"["/app/who\u0000ami"]"#),
    ] {
        let body = format!(
            r#"{{"name":"bad","rootfs":"{rootfs}","mem_mib":256,"vcpus":1,"command":{command},{extra}}}"#
        );
        let response = post_raw(&agent.socket, "/vms", &body).expect("send a raw /vms request");
        assert!(
            response.contains(" 400 ") && response.contains(field),
            "expected a 400 naming {field} for {extra} {command}, got: {response}"
        );
    }
    let ps = agent.ps(true);
    assert!(
        row(&ps, "bad").is_none(),
        "a rejected request left a record:\n{ps}"
    );
    agent.assert_no_cirro_state();
}

/// `nginx:alpine` pinned, so the test boots the same image every time.
const NGINX: &str =
    "nginx:alpine@sha256:df221db836e1754089190208cee7eeda94f233197056426eda74a43ab1abeac2";

/// M4's exit: the CLI pulls an OCI image and builds its rootfs as the
/// caller, and the VM runs the image's own entrypoint and command. Needs
/// network on the first run; the image cache lives in the target dir, so
/// later runs make one manifest request.
#[test]
fn run_boots_an_image_from_a_registry_and_runs_its_command() {
    let Some(agent) = Agent::start(227) else {
        return;
    };
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("image-cache");

    let address = stdout(
        agent
            .cirro()
            .env("XDG_CACHE_HOME", &cache)
            .args(["run", "--name", "nginx", NGINX])
            .assert()
            .success(),
    )
    .trim()
    .to_string();

    let response = wait_for_http(&address, 80);
    assert!(
        response.contains("Welcome to nginx!"),
        "unexpected response from nginx: {response:?}"
    );
    agent
        .cirro()
        .args(["stop", "--force", "nginx"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

/// M5: the agent samples every running VM once a second, and `top --once`
/// prints what each is using.
#[test]
fn top_once_shows_a_busy_vm_using_cpu_and_memory() {
    let Some(agent) = Agent::start(226) else {
        return;
    };
    let address = stdout(
        agent
            .run("busy", &rootfs().guest_init, &["/app/spin"])
            .success(),
    );
    // Disk use comes from the io controller, which jailer doesn't turn on.
    let [_, _, c, d] = address
        .trim()
        .parse::<std::net::Ipv4Addr>()
        .unwrap()
        .octets();
    let io_stat = format!("/sys/fs/cgroup/cirro/cirro-{c:02x}{d:02x}/io.stat");
    assert!(Path::new(&io_stat).exists(), "{io_stat} is missing");

    // A rate needs two samples, a second apart.
    let deadline = Instant::now() + TIMEOUT;
    let (cpu, row) = loop {
        let top = stdout(agent.cirro().args(["top", "--once"]).assert().success());
        let row = row(&top, "busy")
            .unwrap_or_else(|| panic!("top doesn't list busy:\n{top}"))
            .to_string();
        // `-` until the agent has two samples of the VM.
        let cpu: f64 = row
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.trim_end_matches('%').parse().ok())
            .unwrap_or(0.0);
        if cpu >= 50.0 || Instant::now() > deadline {
            break (cpu, row);
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(
        cpu >= 50.0,
        "a VM spinning one vCPU shows {cpu}% CPU: {row}"
    );
    let memory = row.split_whitespace().nth(2).unwrap_or("");
    assert!(
        memory.ends_with('M') && memory != "0M",
        "a running VM shows no memory: {row}"
    );

    agent
        .cirro()
        .args(["stop", "--force", "busy"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}

/// The VM address `cirro run` or `cirro wake` printed, and its last octet.
fn vm_address(printed: &str) -> (String, u8) {
    let address = printed.trim().to_string();
    let host = address
        .parse::<std::net::Ipv4Addr>()
        .unwrap_or_else(|_| panic!("expected a VM address, got {printed:?}"))
        .octets()[3];
    (address, host)
}

/// The number in the counter fixture's `count N` answer.
fn count(response: &str) -> u64 {
    response
        .rsplit_once("count ")
        .and_then(|(_, n)| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("not a counter answer: {response:?}"))
}

/// M6: parking snapshots a VM to disk and frees everything it held on the
/// Node; waking starts a new VM from that snapshot, so the guest carries on
/// where it was instead of booting again.
#[test]
fn a_parked_vm_wakes_where_it_left_off_and_rm_leaves_nothing() {
    let Some(mut agent) = Agent::start(225) else {
        return;
    };
    let (address, host) = vm_address(&stdout(
        agent
            .run("counter", &rootfs().guest_init, &["/app/counter"])
            .success(),
    ));
    wait_for_http(&address, HTTP_PORT);
    let before = count(&http_get(&address, HTTP_PORT).expect("ask the counter"));
    assert!(before >= 2, "the counter should have answered twice by now");

    agent.cirro().args(["park", "counter"]).assert().success();
    assert!(
        host_state_of(225, host).is_empty(),
        "a parked VM still holds host state: {:?}",
        host_state_of(225, host)
    );
    let ps = agent.ps(false);
    assert!(row(&ps, "counter").is_none(), "ps lists a parked VM:\n{ps}");
    let ps = agent.ps(true);
    let parked = row(&ps, "counter").unwrap_or_else(|| panic!("ps -a lacks counter:\n{ps}"));
    assert!(
        parked.contains("parked"),
        "ps -a doesn't say parked: {parked}"
    );

    let (address, _) = vm_address(&stdout(
        agent.cirro().args(["wake", "counter"]).assert().success(),
    ));
    // A guest that booted again would start counting from 1.
    let after = count(&wait_for_http(&address, HTTP_PORT));
    assert_eq!(after, before + 1, "the woken VM didn't carry on counting");
    let ps = agent.ps(false);
    assert!(
        row(&ps, "counter").is_some_and(|r| r.contains(&address)),
        "{ps}"
    );

    // Parked again, from a VM that was itself woken, then discarded.
    agent.cirro().args(["park", "counter"]).assert().success();
    agent.cirro().args(["rm", "counter"]).assert().success();
    let ps = agent.ps(true);
    assert!(
        row(&ps, "counter").is_none(),
        "rm left counter listed:\n{ps}"
    );
    agent.assert_no_cirro_state();
    agent.assert_no_snapshots();
}

/// M6: parked VMs are records like any Ended VM, so they outlive the agent
/// and wake afterwards. A snapshot no record owns, as a park interrupted by
/// a dying agent could leave, is removed when the agent starts again.
#[test]
fn parked_vms_survive_an_agent_restart_and_unowned_snapshots_are_removed() {
    let Some(mut agent) = Agent::start(224) else {
        return;
    };
    let mut kept_count = 0;
    for name in ["kept", "orphan"] {
        let (address, _) = vm_address(&stdout(
            agent
                .run(name, &rootfs().guest_init, &["/app/counter"])
                .success(),
        ));
        wait_for_http(&address, HTTP_PORT);
        let answered = count(&http_get(&address, HTTP_PORT).expect("ask the counter"));
        if name == "kept" {
            kept_count = answered;
        }
        agent.cirro().args(["park", name]).assert().success();
    }

    // Forget `orphan`'s record while the agent is down: the state dir is
    // ours, so a changed copy of the database can be moved over the agent's.
    agent.stop_agent();
    let db = agent.state_dir.join("state.db");
    let changed = agent.state_dir.join("state.db.changed");
    std::fs::copy(&db, &changed).expect("copy the database");
    rusqlite::Connection::open(&changed)
        .and_then(|conn| conn.execute("DELETE FROM vms WHERE name = 'orphan'", []))
        .expect("forget orphan");
    std::fs::rename(&changed, &db).expect("install the changed database");
    agent.spawn().expect("start the agent again");

    let ps = agent.ps(true);
    let kept = row(&ps, "kept").unwrap_or_else(|| panic!("kept was lost:\n{ps}"));
    assert!(
        kept.contains("parked"),
        "kept isn't parked any more: {kept}"
    );
    assert!(row(&ps, "orphan").is_none(), "{ps}");

    let (address, _) = vm_address(&stdout(
        agent.cirro().args(["wake", "kept"]).assert().success(),
    ));
    let after = count(&wait_for_http(&address, HTTP_PORT));
    assert_eq!(after, kept_count + 1, "kept didn't carry on counting");

    agent
        .cirro()
        .args(["stop", "--force", "kept"])
        .assert()
        .success();
    agent.cirro().args(["rm", "kept"]).assert().success();
    agent.assert_no_cirro_state();
    agent.assert_no_snapshots();
}

/// M6: a woken guest's own connections get out at once, before anything has
/// connected in to it. Its network config, gateway MAC included, is the one
/// it was parked with, while every link on the host side is new.
#[test]
fn a_woken_vm_reaches_the_internet_before_anything_reaches_it() {
    let internet: SocketAddr = "1.1.1.1:443".parse().unwrap();
    if !host_reaches(internet) {
        eprintln!("skipping: this host can't reach {internet}, so VM egress can't be judged");
        return;
    }
    let Some(agent) = Agent::start(223) else {
        return;
    };
    agent
        .run(
            "dialer",
            &rootfs().guest_init,
            &["/app/dialer", "1.1.1.1:443"],
        )
        .success();
    wait_for_log(&agent, "dialer", "OPEN");

    agent.cirro().args(["park", "dialer"]).assert().success();
    agent.cirro().args(["wake", "dialer"]).assert().success();
    let woke = Instant::now();
    // Read after the wake, so it may hold the woken guest's first lines too;
    // only lines after it count, which errs towards a slower first dial.
    let parked_log = stdout(agent.cirro().args(["logs", "dialer"]).assert().success());
    let opened = loop {
        let log = stdout(agent.cirro().args(["logs", "dialer"]).assert().success());
        if log[parked_log.len()..].contains("OPEN") {
            break woke.elapsed();
        }
        assert!(
            woke.elapsed() < TIMEOUT,
            "the woken VM never reached {internet}:\n{}",
            &log[parked_log.len()..]
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    eprintln!("first connection out after wake: {opened:?}");
    assert!(
        opened < Duration::from_secs(2),
        "the woken VM took {opened:?} to reach {internet}"
    );

    agent
        .cirro()
        .args(["stop", "--force", "dialer"])
        .assert()
        .success();
    agent.cirro().args(["rm", "dialer"]).assert().success();
    agent.assert_no_cirro_state();
}

/// M6: park, wake, stop and run each refuse a VM in the wrong state, say
/// what to do instead, and leave the VM as it was.
#[test]
fn park_wake_stop_and_run_refuse_vms_in_the_wrong_state() {
    let Some(agent) = Agent::start(222) else {
        return;
    };
    let refused = |args: &[&str], expected: &str| {
        let err = stderr(agent.cirro().args(args).assert().failure());
        assert!(
            err.contains(expected),
            "`cirro {}` should say {expected:?}, got: {err}",
            args.join(" ")
        );
    };
    refused(&["park", "web"], "`cirro ps -a` lists");
    refused(&["wake", "web"], "`cirro ps -a` lists");

    agent
        .run("web", &rootfs().guest_init, &["/app/counter"])
        .success();
    refused(&["wake", "web"], "isn't parked; it's running");

    agent.cirro().args(["park", "web"]).assert().success();
    refused(&["park", "web"], "wake it first");
    refused(&["stop", "web"], "wake it first");
    let err = stderr(
        agent
            .run("web", &rootfs().guest_init, &["/app/counter"])
            .failure(),
    );
    assert!(err.contains("is parked"), "run over a parked name: {err}");
    let ps = agent.ps(true);
    assert!(
        row(&ps, "web").is_some_and(|r| r.contains("parked")),
        "a refusal changed the parked VM:\n{ps}"
    );

    agent.cirro().args(["wake", "web"]).assert().success();
    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    refused(&["park", "web"], "`cirro run` starts");
    refused(&["wake", "web"], "`cirro run` starts");

    agent.cirro().args(["rm", "web"]).assert().success();
    agent.assert_no_cirro_state();
}

/// M6: a parked VM whose snapshot has gone can't be woken, and says so,
/// pointing at `rm`.
#[test]
fn a_parked_vm_whose_snapshot_is_gone_says_so_and_can_be_removed() {
    let Some(mut agent) = Agent::start(221) else {
        return;
    };
    agent
        .run("lost", &rootfs().guest_init, &["/app/counter"])
        .success();
    agent.cirro().args(["park", "lost"]).assert().success();

    // Hide the snapshot while the agent is down. The parked dir is root's,
    // but renaming it within the state dir, which is ours, needs no more.
    agent.stop_agent();
    let parked = agent.state_dir.join("parked");
    let hidden = agent.state_dir.join("parked.hidden");
    std::fs::rename(&parked, &hidden).expect("hide the parked dir");
    agent.spawn().expect("start the agent again");

    let err = stderr(agent.cirro().args(["wake", "lost"]).assert().failure());
    assert!(
        err.contains("snapshot is gone") && err.contains("rm"),
        "wake should say the snapshot is gone and how to remove the VM, got: {err}"
    );
    agent.cirro().args(["rm", "lost"]).assert().success();

    // Put the snapshot back with no record left to own it: the next start
    // removes it.
    agent.stop_agent();
    std::fs::rename(&hidden, &parked).expect("put the parked dir back");
    agent.spawn().expect("start the agent again");
    agent.assert_no_cirro_state();
    agent.assert_no_snapshots();
}

/// M6's exit: `cirro bench` boots, parks, wakes and removes a throwaway VM
/// over and over, and prints each operation's p50 and p99 as the CLI sees
/// them, leaving nothing behind.
#[test]
fn bench_times_boot_park_and_wake_and_leaves_nothing() {
    let Some(mut agent) = Agent::start(220) else {
        return;
    };
    let output = stdout(
        agent
            .cirro()
            .args(["bench", "--runs", "3"])
            .arg(&rootfs().guest_init)
            .args(["--", "/app/counter"])
            .assert()
            .success(),
    );
    for operation in ["boot", "park", "wake"] {
        let row = row(&output, operation)
            .unwrap_or_else(|| panic!("bench doesn't report {operation}:\n{output}"));
        let fields: Vec<&str> = row.split_whitespace().collect();
        let millis = |field: &str| -> f64 {
            field
                .strip_suffix("ms")
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("not a time in ms: {field:?} in {row:?}"))
        };
        assert_eq!(fields.get(1), Some(&"3"), "runs column: {row}");
        let (p50, p99) = (millis(fields[2]), millis(fields[3]));
        assert!(p50 > 0.0 && p99 >= p50, "{operation}: p50 {p50}, p99 {p99}");
    }
    let ps = agent.ps(true);
    assert_eq!(ps.lines().count(), 1, "bench left VMs behind:\n{ps}");
    agent.assert_no_cirro_state();
    agent.assert_no_snapshots();
}

/// Replacing the agent's binary on disk, as a rebuild or an upgrade does,
/// doesn't stop the running agent starting VMs: it opens each rootfs through
/// a helper that runs the agent's own binary, which must be the one running,
/// not whatever is at its path now.
#[test]
fn a_running_agent_still_starts_vms_after_its_binary_is_replaced() {
    let Some(agent) = Agent::start(218) else {
        return;
    };
    // A new file at the same path, as `cargo build` or a package upgrade
    // leaves it: the running agent's binary is now a deleted inode.
    let path = test_agent_path();
    let replacement = path.with_extension(format!("replacement-{}", std::process::id()));
    std::fs::copy(&path, &replacement).expect("copy the agent binary");
    if let Err(e) = std::fs::rename(&replacement, &path) {
        let _ = std::fs::remove_file(&replacement);
        panic!("replace the agent binary: {e}");
    }

    let address = stdout(
        agent
            .run("web", &rootfs().guest_init, &["/app/http_app"])
            .success(),
    );
    assert!(wait_for_http(address.trim(), HTTP_PORT).contains("hello from cirro"));
    agent
        .cirro()
        .args(["stop", "--force", "web"])
        .assert()
        .success();
    agent.assert_no_cirro_state();
}
