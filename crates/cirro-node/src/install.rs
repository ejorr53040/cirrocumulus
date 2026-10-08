//! `cirro node install` / `cirro node uninstall` (#11): sets up (and tears
//! back down) everything a Node needs before `cirro node agent` can run --
//! prerequisites, the pinned Firecracker/jailer/kernel, the `cirro` group,
//! the state dir, a host firewall rule letting VMs' traffic be forwarded,
//! and a systemd unit that starts the agent at boot.
//!
//! Everything install writes is recorded in one `NodeConfig` file at
//! `<state_dir>/node.json`, so uninstall doesn't need to be told the same
//! group/subnet/unit name again -- it reads back what install chose. This
//! is also what makes a repeat `install` a no-op: called again with the
//! same arguments, every step below finds its target already in the state
//! it would have created and does nothing.

use crate::egress;
use crate::release::{self, ReleaseBinaries};
use crate::state::{Record, Store};
use crate::subnet::Subnet;
use crate::vm;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const IP_FORWARD_RECORD: &str = "ip_forward.before";
const NODE_CONFIG_FILE: &str = "node.json";
const RELEASE_SUBDIR: &str = "release";

/// What `cirro node install` was given.
pub struct InstallConfig {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    /// The group created (if missing) and used for the agent socket.
    pub group: String,
    pub subnet: Subnet,
    /// The systemd unit's name, without `.service`.
    pub unit_name: String,
    /// Skips fetching the pinned release when given -- the same convention
    /// `node agent`'s own `--firecracker`/`--jailer`/`--kernel` flags use,
    /// so tests can point install at local fixtures instead of the network.
    pub release_override: Option<ReleaseBinaries>,
    /// The edge the agent serves, passed on to `cirro node agent`.
    pub edge: EdgeConfig,
}

/// Where the agent's edge listens, and whether it gets ACME certificates:
/// `cirro node agent`'s `--http`, `--https` and `--acme-email`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EdgeConfig {
    pub http: Option<std::net::SocketAddr>,
    pub https: Option<std::net::SocketAddr>,
    pub acme_email: Option<String>,
}

impl EdgeConfig {
    /// The `cirro node agent` arguments for it, each with a leading space.
    fn agent_args(&self) -> String {
        let mut args = String::new();
        if let Some(http) = self.http {
            args.push_str(&format!(" --http {http}"));
        }
        if let Some(https) = self.https {
            args.push_str(&format!(" --https {https}"));
        }
        if let Some(email) = &self.acme_email {
            args.push_str(&format!(" --acme-email {email}"));
        }
        args
    }
}

/// Everything install resolved, persisted so uninstall (and a future
/// `cirro node agent` launched by the systemd unit) can find it again
/// without being told the same arguments twice.
#[derive(Serialize, Deserialize)]
struct NodeConfig {
    socket: PathBuf,
    group: String,
    subnet: String,
    unit_name: String,
    #[serde(flatten)]
    release: ReleaseBinaries,
    /// Missing from a Node config written before the edge (M7).
    #[serde(default)]
    edge: EdgeConfig,
}

/// Checks KVM and cgroup v2 -- the successor to `scripts/step0/prereqs.sh`'s
/// checks of the same two things (same `stat -f -c %T` test that script
/// uses, rather than a raw `statfs(2)` FFI call, for the same reason this
/// crate already shells out to `ip`/`nft` elsewhere). Runs first and
/// changes nothing, so a Node that fails here is exactly as it was before
/// `install` was called.
pub fn check_prereqs() -> io::Result<()> {
    let kvm = Path::new("/dev/kvm");
    if !(kvm.exists() && is_rw(kvm)) {
        return Err(io::Error::other(
            "/dev/kvm is missing or not read/write for this user: Firecracker needs KVM",
        ));
    }
    if cgroup_fs_type("/sys/fs/cgroup").as_deref() != Some("cgroup2fs") {
        return Err(io::Error::other(
            "/sys/fs/cgroup is not a unified cgroup v2 hierarchy: jailer needs --cgroup-version=2, \
             which needs the v2 hierarchy",
        ));
    }
    Ok(())
}

fn is_rw(path: &Path) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_ok()
}

fn cgroup_fs_type(path: &str) -> Option<String> {
    let output = Command::new("stat")
        .args(["-f", "-c", "%T", path])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Sets up everything `cirro node agent` needs: prerequisites, the pinned
/// release (unless overridden), the group, the state dir, the Node config
/// and a systemd unit that starts the agent at boot. Idempotent -- see the
/// module doc.
pub fn install(cfg: &InstallConfig) -> io::Result<()> {
    check_prereqs()?;

    fs::create_dir_all(&cfg.state_dir)?;

    let release = match &cfg.release_override {
        Some(r) => r.clone(),
        None => {
            let release_dir = cfg.state_dir.join(RELEASE_SUBDIR);
            fs::create_dir_all(&release_dir)?;
            release::fetch(&release_dir)?
        }
    };

    ensure_group(&cfg.group)?;

    let node_config = NodeConfig {
        socket: cfg.socket.clone(),
        group: cfg.group.clone(),
        subnet: cfg.subnet.to_string(),
        unit_name: cfg.unit_name.clone(),
        release,
        edge: cfg.edge.clone(),
    };
    write_node_config(&cfg.state_dir, &node_config)?;

    allow_through_host_firewall(&node_config.subnet)?;

    install_unit(&node_config, &cfg.state_dir)?;

    Ok(())
}

/// Reverses [`install`]: refuses while a VM is still running unless
/// `force`, which kills them, then removes the systemd unit, the group,
/// the host firewall rule, the egress policy, restores
/// `net.ipv4.ip_forward`, and removes the state dir -- everything
/// install created, and everything the agent itself created while it ran
/// (the egress table, `ip_forward.before`).
pub async fn uninstall(state_dir: &Path, force: bool) -> io::Result<()> {
    let node_config = read_node_config(state_dir).map_err(|e| {
        io::Error::other(format!(
            "{}: {e} (nothing installed here?)",
            state_dir.display()
        ))
    })?;

    if !force {
        refuse_if_vms_running(state_dir, &node_config.subnet)?;
    }

    remove_unit(&node_config.unit_name)?;
    // Once the agent is stopped, so it can't take them back, and before the
    // egress policy goes: a VM left running would outlive it, free to reach
    // the LAN.
    clean_up_vms(state_dir, &node_config).await?;
    let _ = remove_group(&node_config.group);
    remove_from_host_firewall(&node_config.subnet);
    let _ = egress::remove_node_policy();
    egress::restore_ip_forward(&state_dir.join(IP_FORWARD_RECORD))?;

    fs::remove_dir_all(state_dir)?;
    Ok(())
}

/// The records of every VM in `state_dir`, none before the agent first ran.
fn records(state_dir: &Path, subnet: &str) -> io::Result<Vec<Record>> {
    let db = state_dir.join("state.db");
    if !db.exists() {
        return Ok(Vec::new());
    }
    Store::open(&db, subnet)?.load()
}

/// Kills every VM the Node has a record of and removes its host state, as
/// the agent does for a VM that is gone. Ended VMs too: one whose VMM died
/// can still have its namespace, veth or cgroup until the agent next
/// starts, and after uninstall it never will.
async fn clean_up_vms(state_dir: &Path, config: &NodeConfig) -> io::Result<()> {
    let subnet: Subnet = config.subnet.parse().map_err(io::Error::other)?;
    let node = vm::NodeConfig {
        firecracker: config.release.firecracker.clone(),
        jailer: config.release.jailer.clone(),
        kernel: config.release.kernel.clone(),
        jail_base: vm::jail_base(state_dir),
        node_address: subnet.node_address(),
        // Only used to hand a new VM's console log to the group.
        cirro_gid: 0,
    };
    for record in records(state_dir, &config.subnet)? {
        if let Some(vm_address) = record.info.vm_address {
            vm::clean_up(&node, vm_address).await;
        }
    }
    Ok(())
}

fn refuse_if_vms_running(state_dir: &Path, subnet: &str) -> io::Result<()> {
    let running: Vec<String> = records(state_dir, subnet)?
        .into_iter()
        .filter(|r| r.process.is_some_and(|p| p.is_running()))
        .map(|r| r.info.name)
        .collect();
    if running.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "refusing to uninstall: {} VM(s) still running ({}); stop them first or pass \
             --force",
            running.len(),
            running.join(", ")
        )))
    }
}

/// `ensure_group`/`remove_group` are mirror images of each other: check
/// whether `name` already matches the wanted existence state, and if not,
/// run the command that would fix it.
fn set_group_existence(name: &str, want_exists: bool, verb: &str) -> io::Result<()> {
    if nix::unistd::Group::from_name(name)?.is_some() == want_exists {
        return Ok(());
    }
    run(Command::new(verb).arg(name))
}

fn ensure_group(name: &str) -> io::Result<()> {
    set_group_existence(name, true, "groupadd")
}

/// Best-effort: a group that's already gone (or never created, e.g. an
/// install that only ever used a numeric gid) isn't a failure to undo.
fn remove_group(name: &str) -> io::Result<()> {
    set_group_existence(name, false, "groupdel")
}

/// The comment ufw shows beside the rule, so `ufw status` says whose it is.
const UFW_COMMENT: &str = "cirro Node VMs";

/// Lets the Node subnet's forwarded traffic past a host firewall whose
/// default is to drop it: ufw's `DEFAULT_FORWARD_POLICY="DROP"`, or
/// firewalld's zones. The egress policy's own drops (SMTP, other VMs, the
/// LAN, the Node) still apply, since a drop is final across tables (see
/// `egress`'s module doc); this only stops the host firewall dropping the
/// rest, the internet traffic VMs are meant to have. Fails install if a
/// firewall that is there refuses the rule.
fn allow_through_host_firewall(subnet: &str) -> io::Result<()> {
    for mut command in host_firewall_commands(subnet, true) {
        run(&mut command)?;
    }
    Ok(())
}

/// Reverses [`allow_through_host_firewall`]. Best-effort, like the rest
/// of uninstall: a rule that's already gone isn't a failure.
fn remove_from_host_firewall(subnet: &str) {
    for mut command in host_firewall_commands(subnet, false) {
        let _ = run(&mut command);
    }
}

/// The commands that add (`allow`) or remove the Node subnet's rule.
///
/// The ufw rule matches traffic in on the VMs' veths from the subnet, with
/// no egress interface, so it keeps working when the default route moves
/// (Wi-Fi to Ethernet). It is there whenever ufw is installed, even
/// inactive, so turning ufw on later doesn't cut VMs off. firewalld gets
/// the subnet as a source of its `trusted` zone, at runtime and in its
/// saved config while it runs, and in its saved config alone through
/// `firewall-offline-cmd` while it doesn't. Adding a rule that's there is
/// a no-op for both.
fn host_firewall_commands(subnet: &str, allow: bool) -> Vec<Command> {
    let mut commands = Vec::new();
    if succeeds("ufw", "version") {
        let mut ufw = Command::new("ufw");
        ufw.args(ufw_route_rule(subnet, allow));
        commands.push(ufw);
    }
    let source = format!("--{}-source={subnet}", if allow { "add" } else { "remove" });
    let firewalld = |program: &str, permanent: bool| {
        let mut command = Command::new(program);
        command.args(permanent.then_some("--permanent"));
        command.args(["--zone=trusted", &source]);
        command
    };
    if succeeds("firewall-cmd", "--state") {
        commands.push(firewalld("firewall-cmd", false));
        commands.push(firewalld("firewall-cmd", true));
    } else if succeeds("firewall-offline-cmd", "--version") {
        commands.push(firewalld("firewall-offline-cmd", false));
    }
    commands
}

/// `ufw route [delete] allow ...` for the Node subnet.
fn ufw_route_rule(subnet: &str, allow: bool) -> Vec<&str> {
    let mut args = vec!["route"];
    if !allow {
        args.push("delete");
    }
    args.extend(["allow", "in", "on", "cirro-+", "from", subnet]);
    args.extend(["comment", UFW_COMMENT]);
    args
}

/// Whether `program arg` runs and exits 0.
fn succeeds(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn write_node_config(state_dir: &Path, config: &NodeConfig) -> io::Result<()> {
    let json = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    fs::write(state_dir.join(NODE_CONFIG_FILE), json)
}

fn read_node_config(state_dir: &Path) -> io::Result<NodeConfig> {
    let json = fs::read_to_string(state_dir.join(NODE_CONFIG_FILE))?;
    serde_json::from_str(&json).map_err(io::Error::other)
}

/// Writes `<unit_name>.service`, running the agent with the paths
/// `install` resolved, then `daemon-reload`s and enables it. Re-running
/// with identical config writes the identical unit content and re-enables
/// an already-enabled unit, both no-ops.
fn install_unit(config: &NodeConfig, state_dir: &Path) -> io::Result<()> {
    let cirro_bin = std::env::current_exe()?;
    let unit = format!(
        "[Unit]\n\
         Description=Cirrocumulus Node agent\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={bin} node agent --socket {socket} --socket-group {group} \
         --subnet {subnet} --state-dir {state_dir} --firecracker {firecracker} \
         --jailer {jailer} --kernel {kernel}{edge}\n\
         Restart=on-failure\n\
         LimitNOFILE=65536\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = cirro_bin.display(),
        socket = config.socket.display(),
        group = config.group,
        subnet = config.subnet,
        state_dir = state_dir.display(),
        firecracker = config.release.firecracker.display(),
        jailer = config.release.jailer.display(),
        kernel = config.release.kernel.display(),
        edge = config.edge.agent_args(),
    );
    let unit_path = unit_path(&config.unit_name);
    fs::write(&unit_path, unit)?;
    run(Command::new("systemctl").arg("daemon-reload"))?;
    run(Command::new("systemctl")
        .args(["enable", "--now"])
        .arg(format!("{}.service", config.unit_name)))
}

/// Stops, disables and removes the unit. Missing/not-enabled is fine --
/// matches this module's general tolerance for "already undone".
fn remove_unit(unit_name: &str) -> io::Result<()> {
    let service = format!("{unit_name}.service");
    let _ = Command::new("systemctl").args(["stop", &service]).status();
    let _ = Command::new("systemctl")
        .args(["disable", &service])
        .status();
    let path = unit_path(unit_name);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    run(Command::new("systemctl").arg("daemon-reload"))
}

fn unit_path(unit_name: &str) -> PathBuf {
    PathBuf::from("/etc/systemd/system").join(format!("{unit_name}.service"))
}

fn run(command: &mut Command) -> io::Result<()> {
    release::run_capturing(command).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::ProcessId;
    use cirro_proto::{EndReason, Ended, VmInfo};

    /// Needs no root at all: a real SQLite state dir and the test's own
    /// process (a real, genuinely running pid) standing in for a VM's
    /// VMM, so [`ProcessId::is_running`] sees the truth without mocking
    /// it. Exercises the refusal decision `uninstall` makes, independent
    /// of the group/systemd-unit mutation the full CLI path needs root
    /// for (covered instead by `crates/cirro/tests/node_install.rs`).
    fn tempdir() -> PathBuf {
        crate::test_util::tempdir("cirro-install-test")
    }

    #[test]
    fn ufw_rule_routes_the_subnet_in_from_vm_veths_to_anywhere() {
        assert_eq!(
            ufw_route_rule("10.77.0.0/24", true).join(" "),
            "route allow in on cirro-+ from 10.77.0.0/24 comment cirro Node VMs"
        );
    }

    #[test]
    fn ufw_delete_names_the_same_rule_install_added() {
        let added = ufw_route_rule("10.77.0.0/24", true);
        let deleted = ufw_route_rule("10.77.0.0/24", false);
        assert_eq!(deleted[..2], ["route", "delete"]);
        assert_eq!(deleted[2..], added[1..]);
    }

    #[test]
    fn does_not_refuse_when_the_state_db_does_not_exist_yet() {
        let dir = tempdir();
        refuse_if_vms_running(&dir, "10.77.0.0/24").expect("no db means nothing is running");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn does_not_refuse_when_every_vm_has_ended() {
        let dir = tempdir();
        let store = Store::open(&dir.join("state.db"), "10.77.0.0/24").unwrap();
        let info = VmInfo {
            name: "web".to_string(),
            vm_address: None,
            mem_mib: 256,
            vcpus: 1,
            started_at: 0,
            ended: None,
            route: None,
        };
        let process = ProcessId::of(std::process::id()).expect("this test process exists");
        store
            .insert_running(&info, &dir.join("web.log"), process)
            .unwrap();
        store
            .mark_ended(
                "web",
                &Ended {
                    at: 0,
                    reason: EndReason::Graceful,
                },
            )
            .unwrap();
        drop(store);

        refuse_if_vms_running(&dir, "10.77.0.0/24").expect("an Ended VM isn't a running one");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_while_a_vm_is_running() {
        let dir = tempdir();
        let store = Store::open(&dir.join("state.db"), "10.77.0.0/24").unwrap();
        let info = VmInfo {
            name: "web".to_string(),
            vm_address: None,
            mem_mib: 256,
            vcpus: 1,
            started_at: 0,
            ended: None,
            route: None,
        };
        // This test process is real and running, so `ProcessId::is_running`
        // sees a genuinely live process -- exactly what a running VM's
        // record looks like from `uninstall`'s point of view.
        let process = ProcessId::of(std::process::id()).expect("this test process exists");
        store
            .insert_running(&info, &dir.join("web.log"), process)
            .unwrap();
        drop(store);

        let err = refuse_if_vms_running(&dir, "10.77.0.0/24").unwrap_err();
        assert!(err.to_string().contains("web"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
