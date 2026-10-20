//! `cirro node install` / `cirro node uninstall` (#11), at the same CLI
//! seam as `crates/cirro/tests/node_agent.rs`: a throwaway install under
//! `sudo -n`, with its own test-scoped group, systemd unit name and state
//! dir, so it never touches whatever a real `cirro` group/unit/state dir
//! this host may eventually have.
//!
//! The refusal-while-a-VM-is-running decision itself (and its `--force`
//! override) is unit-tested directly in `cirro_node::install`, against a
//! real SQLite state dir and the test process's own live pid standing in
//! for a VM's VMM -- no root needed there. This file covers the parts
//! that unit test can't: the real group/systemd-unit/state-dir mutation,
//! run through the CLI exactly as a person would.
//!
//! Skipped (not failed) under the same conditions as `node_agent.rs`: see
//! that file's module doc for the sudoers rule this needs.

use assert_cmd::Command;
use std::path::PathBuf;

mod common;
use common::{home, latest_kernel, repo_root, rootfs, test_agent, test_agent_path};

/// One test's throwaway install target: a unique group, unit name, subnet
/// and state dir, all named from this test's pid so parallel test runs
/// (and parallel `cargo test` invocations across this file and
/// `node_agent.rs`) never collide. `Drop` force-uninstalls, so a failed
/// assertion never strands a real group/systemd unit/state dir on the
/// host.
struct Install {
    state_dir: PathBuf,
    group: String,
    unit_name: String,
    subnet: String,
    /// The edge's ports are `18000 + edge` and `19000 + edge`.
    edge: u16,
    installed: bool,
}

impl Install {
    fn new(label: &str, edge: u16) -> Option<Install> {
        if test_agent().is_none() {
            eprintln!(
                "skipping: `sudo -n {} --version` failed -- see this test's module doc",
                test_agent_path().display()
            );
            return None;
        }
        let pid = std::process::id();
        Some(Install {
            state_dir: home().join(format!(".cache/cirro-install-test-{label}-{pid}")),
            group: format!("cirro-test-{label}-{pid}"),
            unit_name: format!("cirro-test-{label}-{pid}"),
            subnet: format!("10.78.{}.0/24", pid % 200 + 20),
            edge,
            installed: false,
        })
    }

    fn install_cmd(&self) -> std::process::Command {
        let agent_bin = test_agent().expect("checked at construction");
        let repo = repo_root();
        let mut cmd = std::process::Command::new("sudo");
        cmd.arg("-n")
            .arg(agent_bin)
            .args(["node", "install", "--state-dir"])
            .arg(&self.state_dir)
            .arg("--socket")
            .arg(self.socket())
            .args(["--group", &self.group])
            .args(["--unit-name", &self.unit_name])
            .args(["--subnet", &self.subnet])
            .arg("--firecracker")
            .arg(repo.join("firecracker"))
            .arg("--jailer")
            .arg(repo.join("jailer"))
            .arg("--kernel")
            .arg(latest_kernel())
            .args(["--http", &format!("127.0.0.1:{}", 18000 + self.edge)])
            .args(["--https", &format!("127.0.0.1:{}", 19000 + self.edge)]);
        cmd
    }

    fn install(&mut self) -> Result<(), String> {
        let output = self.install_cmd().output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        self.installed = true;
        Ok(())
    }

    fn uninstall(&self, force: bool) -> Result<(), String> {
        let agent_bin = test_agent().expect("checked at construction");
        let mut cmd = std::process::Command::new("sudo");
        cmd.arg("-n")
            .arg(agent_bin)
            .args(["node", "uninstall", "--state-dir"])
            .arg(&self.state_dir);
        if force {
            cmd.arg("--force");
        }
        let output = cmd.output().map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    fn socket(&self) -> PathBuf {
        self.state_dir.join("agent.sock")
    }

    /// The names of the namespaces and cgroups VMs in this install's subnet
    /// hold: `cirro-` and the low 16 bits of the VM address, in hex.
    fn vm_host_state(&self) -> Vec<String> {
        let third: u8 = self.subnet.split('.').nth(2).unwrap().parse().unwrap();
        let prefix = format!("cirro-{third:02x}");
        ["/run/netns", "/sys/fs/cgroup/cirro"]
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten())
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with(&prefix) && name.len() == prefix.len() + 2)
            .collect()
    }

    fn unit_file(&self) -> PathBuf {
        PathBuf::from("/etc/systemd/system").join(format!("{}.service", self.unit_name))
    }

    fn is_enabled(&self) -> bool {
        std::process::Command::new("systemctl")
            .args(["is-enabled", &format!("{}.service", self.unit_name)])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
    }

    /// Whether ufw's saved rules hold one routing this install's subnet,
    /// or `None` on a host without ufw. Reads the rules file rather than
    /// `ufw status`, which needs root.
    fn ufw_routes_subnet(&self) -> Option<bool> {
        let rules = std::fs::read_to_string("/etc/ufw/user.rules").ok()?;
        Some(
            rules
                .lines()
                .any(|l| l.starts_with("### tuple ### route:allow") && l.contains(&self.subnet)),
        )
    }

    fn group_exists(&self) -> bool {
        std::process::Command::new("getent")
            .args(["group", &self.group])
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        if self.installed {
            let _ = self.uninstall(true);
        }
    }
}

#[test]
fn install_creates_a_group_state_dir_and_enabled_unit_and_is_idempotent() {
    let Some(mut install) = Install::new("basic", 299) else {
        return;
    };

    install.install().expect("first install");
    assert!(install.group_exists(), "group not created");
    assert!(
        install.state_dir.join("node.json").exists(),
        "node.json not written"
    );
    assert!(install.is_enabled(), "unit not enabled");
    let unit_contents = std::fs::read_to_string(install.unit_file()).expect("read unit file");
    assert!(
        unit_contents.contains("ExecStart="),
        "unit missing ExecStart: {unit_contents}"
    );
    assert!(
        unit_contents.contains(&install.state_dir.display().to_string()),
        "unit doesn't reference the state dir: {unit_contents}"
    );
    assert!(
        unit_contents.contains("--http 127.0.0.1:18299 --https 127.0.0.1:19299"),
        "unit doesn't start the agent's edge where install was told: {unit_contents}"
    );
    assert_ne!(
        install.ufw_routes_subnet(),
        Some(false),
        "ufw is installed but has no rule letting the subnet's traffic be forwarded"
    );

    // Re-running install with the same arguments is a no-op: it succeeds
    // again rather than erring on "already exists".
    install.install().expect("second install (idempotent)");
    assert!(install.group_exists());
    assert!(install.is_enabled());

    install.uninstall(false).expect("uninstall");
    install.installed = false; // Drop shouldn't try again.
    assert!(!install.group_exists(), "group survived uninstall");
    assert!(
        !install.unit_file().exists(),
        "unit file survived uninstall"
    );
    assert!(!install.is_enabled(), "unit still enabled after uninstall");
    assert!(!install.state_dir.exists(), "state dir survived uninstall");
    assert_ne!(
        install.ufw_routes_subnet(),
        Some(true),
        "ufw rule survived uninstall"
    );
}

#[test]
fn uninstall_of_a_state_dir_that_was_never_installed_fails_clearly() {
    let Some(agent_bin) = test_agent() else {
        eprintln!("skipping: no test agent");
        return;
    };
    let never_installed = home().join(format!(
        ".cache/cirro-install-test-missing-{}",
        std::process::id()
    ));
    let output = std::process::Command::new("sudo")
        .arg("-n")
        .arg(agent_bin)
        .args(["node", "uninstall", "--state-dir"])
        .arg(&never_installed)
        .output()
        .expect("run cirro node uninstall");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing installed"),
        "unexpected error: {stderr}"
    );
}

/// `Command::cargo_bin` (unprivileged) rejects a bad `--firecracker`/
/// `--jailer`/`--kernel` combination before anything runs as root: `clap`
/// itself enforces the `requires` relationships between them.
#[test]
fn install_requires_firecracker_jailer_and_kernel_together() {
    let mut cmd = Command::cargo_bin("cirro").unwrap();
    cmd.args(["node", "install", "--firecracker", "/tmp/nonexistent"]);
    cmd.assert().failure();
}

#[test]
fn uninstall_force_kills_the_nodes_running_vms() {
    let Some(mut install) = Install::new("force", 298) else {
        return;
    };
    install.install().expect("install");

    // As root: the test user isn't in the install's own group.
    let run = std::process::Command::new("sudo")
        .arg("-n")
        .arg(test_agent().expect("checked at construction"))
        .arg("--socket")
        .arg(install.socket())
        .args(["run", "--name", "web"])
        .arg(&rootfs().guest_init)
        .args(["--", "/app/http_app"])
        .output()
        .expect("run cirro run");
    assert!(
        run.status.success(),
        "cirro run: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        !install.vm_host_state().is_empty(),
        "the running VM has no namespace or cgroup to look for"
    );

    install
        .uninstall(false)
        .expect_err("uninstall without --force");
    install.uninstall(true).expect("uninstall --force");
    install.installed = false;
    assert_eq!(
        install.vm_host_state(),
        Vec::<String>::new(),
        "uninstall --force left the VM running"
    );
    assert!(!install.state_dir.exists(), "state dir survived uninstall");
}
