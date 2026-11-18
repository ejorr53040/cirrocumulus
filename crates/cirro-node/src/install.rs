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

use crate::cmd;
use crate::egress;
use crate::layout;
use crate::release::{self, ReleaseBinaries};
use crate::state::{Record, Store};
use crate::subnet::Subnet;
use crate::vm;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a Node keeps its state unless told otherwise.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/cirro";
/// The group allowed to use the agent socket unless told otherwise.
pub const DEFAULT_GROUP: &str = "cirro";
/// The Node subnet unless told otherwise.
pub const DEFAULT_SUBNET: &str = "10.77.0.0/24";

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
#[derive(Clone, Serialize, Deserialize)]
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
            args.push_str(&format!(" --acme-email {}", unit_arg(email)));
        }
        args
    }
}

/// `arg` as one word of a systemd `ExecStart=` line. systemd splits that on
/// whitespace and expands `%` specifiers and `$` variables even inside
/// quotes, so an argument with any of them is quoted and escaped.
fn unit_arg(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:=,@+".contains(c))
    {
        return arg.to_string();
    }
    let escaped = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// Everything install resolved, persisted so uninstall can find it again
/// without being told the same arguments twice. The agent the systemd unit
/// launches doesn't read it: it gets its arguments on the unit's command line.
#[derive(Serialize, Deserialize)]
struct NodeConfig {
    socket: PathBuf,
    group: String,
    subnet: String,
    unit_name: String,
    #[serde(flatten)]
    release: ReleaseBinaries,
    edge: EdgeConfig,
    /// Whether install created `group` (so uninstall may delete it), rather
    /// than finding it already there. Absent from a `node.json` written
    /// before this field existed, which reads as `false`: no group removed.
    #[serde(default)]
    group_created: bool,
}

/// Checks KVM and cgroup v2 -- the successor to `scripts/step0/prereqs.sh`'s
/// checks of the same two things (same `stat -f -c %T` test that script
/// uses, rather than a raw `statfs(2)` FFI call, for the same reason this
/// crate already shells out to `ip`/`nft` elsewhere). Runs first and
/// changes nothing, so a Node that fails here is exactly as it was before
/// `install` was called.
fn check_prereqs() -> io::Result<()> {
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
    // Only Machines need it, so its absence doesn't stop the install.
    if Command::new("ssh").arg("-V").output().is_err() {
        eprintln!(
            "cirro: warning: no `ssh` found; Machines (`cirro machine add`) need OpenSSH's \
             ssh, ssh-keygen and ssh-keyscan"
        );
    }

    fs::create_dir_all(&cfg.state_dir)?;

    let release = match &cfg.release_override {
        Some(r) => installed_release(&cfg.state_dir, r)?,
        None => {
            let release_dir = layout::release(&cfg.state_dir);
            fs::create_dir_all(&release_dir)?;
            release::fetch(&release_dir)?
        }
    };

    let previous = read_node_config(&cfg.state_dir).ok();
    let group_created = ensure_group(&cfg.group)?
        || previous.is_some_and(|p| p.group_created && p.group == cfg.group);

    let node_config = NodeConfig {
        socket: cfg.socket.clone(),
        group: cfg.group.clone(),
        subnet: cfg.subnet.to_string(),
        unit_name: cfg.unit_name.clone(),
        release,
        edge: cfg.edge.clone(),
        group_created,
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
    // Only a group install created: a pre-existing one may have other members.
    if node_config.group_created {
        let _ = remove_group(&node_config.group);
    }
    remove_from_host_firewall(&node_config.subnet);
    let _ = egress::remove_node_policy();
    egress::restore_ip_forward(&layout::ip_forward_record(state_dir))?;

    fs::remove_dir_all(state_dir)?;
    Ok(())
}

/// The records of every VM in `state_dir`, none before the agent first ran.
fn records(state_dir: &Path, subnet: &str) -> io::Result<Vec<Record>> {
    let db = layout::state_db(state_dir);
    if !db.exists() {
        return Ok(Vec::new());
    }
    Store::open(&db, subnet)?.load()
}

/// Kills every VM in the Node subnet and removes its host state, recorded
/// or not, as the agent's own sweep does for VMs that are gone: after
/// uninstall, no agent ever will.
async fn clean_up_vms(state_dir: &Path, config: &NodeConfig) -> io::Result<()> {
    let subnet: Subnet = config.subnet.parse().map_err(io::Error::other)?;
    let jails = vm::jails(&layout::jail_base(state_dir), &config.release.firecracker);
    for vm_address in vm::host_state_in(&jails, &subnet) {
        vm::clean_up(&jails, vm_address).await;
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

fn ensure_group(name: &str) -> io::Result<bool> {
    if nix::unistd::Group::from_name(name)?.is_some() {
        return Ok(false);
    }
    cmd::run(Command::new("groupadd").arg(name), None)?;
    Ok(true)
}

/// Deletes the group with `groupdel`. A group that's already gone (or never
/// created, e.g. an install that only ever used a numeric gid) is `Ok`;
/// only a failing `groupdel` is an error.
fn remove_group(name: &str) -> io::Result<()> {
    if nix::unistd::Group::from_name(name)?.is_some() {
        cmd::run(Command::new("groupdel").arg(name), None)?;
    }
    Ok(())
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
        cmd::run(&mut command, None)?;
    }
    Ok(())
}

/// Reverses [`allow_through_host_firewall`]. Best-effort, like the rest
/// of uninstall: a rule that's already gone isn't a failure.
fn remove_from_host_firewall(subnet: &str) {
    for mut command in host_firewall_commands(subnet, false) {
        let _ = cmd::run(&mut command, None);
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
fn ufw_route_rule(subnet: &str, allow: bool) -> Vec<String> {
    let mut args = vec!["route"];
    if !allow {
        args.push("delete");
    }
    let vm_veths = format!("{}+", vm::HOST_ID_PREFIX);
    args.extend(["allow", "in", "on", &vm_veths, "from", subnet]);
    args.extend(["comment", UFW_COMMENT]);
    args.into_iter().map(String::from).collect()
}

fn succeeds(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Root's alone, from the moment it exists: it names the Node's group,
/// subnet and ACME email, which are no business of other local users.
fn write_node_config(state_dir: &Path, config: &NodeConfig) -> io::Result<()> {
    let json = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    let path = layout::node_config(state_dir);
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?
        .write_all(json.as_bytes())?;
    // An earlier install wrote it 0644.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
}

fn read_node_config(state_dir: &Path) -> io::Result<NodeConfig> {
    let json = fs::read_to_string(layout::node_config(state_dir))?;
    serde_json::from_str(&json).map_err(io::Error::other)
}

/// Writes `<unit_name>.service`, running the agent with the paths
/// `install` resolved, then `daemon-reload`s and enables it. Re-running
/// with identical config writes the identical unit content and re-enables
/// an already-enabled unit, both no-ops.
fn install_unit(config: &NodeConfig, state_dir: &Path) -> io::Result<()> {
    let cirro_bin = copy_root_owned(
        &std::env::current_exe()?,
        &layout::bin(state_dir),
        "cirro",
        0o755,
    )?;
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
        bin = unit_arg(&cirro_bin.to_string_lossy()),
        socket = unit_arg(&config.socket.to_string_lossy()),
        group = unit_arg(&config.group),
        subnet = unit_arg(&config.subnet),
        state_dir = unit_arg(&state_dir.to_string_lossy()),
        firecracker = unit_arg(&config.release.firecracker.to_string_lossy()),
        jailer = unit_arg(&config.release.jailer.to_string_lossy()),
        kernel = unit_arg(&config.release.kernel.to_string_lossy()),
        edge = config.edge.agent_args(),
    );
    let unit_path = unit_path(&config.unit_name);
    fs::write(&unit_path, unit)?;
    cmd::run(Command::new("systemctl").arg("daemon-reload"), None)?;
    cmd::run(
        Command::new("systemctl")
            .args(["enable", "--now"])
            .arg(format!("{}.service", config.unit_name)),
        None,
    )
    .map(drop)
}

/// Copies `src` to `dir/name` with `mode` and returns the copy's path. The
/// copy is what root runs, so `dir` is 0700 and nothing another user can
/// write. Staged and renamed into place: a running agent has the old copy
/// open, and writing over it fails (ETXTBSY).
fn copy_root_owned(src: &Path, dir: &Path, name: &str, mode: u32) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let installed = dir.join(name);
    let staged = dir.join(format!("{name}.new"));
    fs::copy(src, &staged)?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(mode))?;
    fs::rename(&staged, &installed)?;
    Ok(installed)
}

/// Copies an override's firecracker, jailer and kernel into the state dir's
/// release dir, where [`release::fetch`] puts the pinned ones, and returns
/// the copies. The unit and the agent run these, so whoever owns the source
/// file can't change what root runs.
fn installed_release(state_dir: &Path, src: &ReleaseBinaries) -> io::Result<ReleaseBinaries> {
    let dir = layout::release(state_dir);
    Ok(ReleaseBinaries {
        firecracker: copy_root_owned(&src.firecracker, &dir, "firecracker", 0o755)?,
        jailer: copy_root_owned(&src.jailer, &dir, "jailer", 0o755)?,
        kernel: copy_root_owned(&src.kernel, &dir, "kernel", 0o644)?,
    })
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
    cmd::run(Command::new("systemctl").arg("daemon-reload"), None).map(drop)
}

fn unit_path(unit_name: &str) -> PathBuf {
    PathBuf::from("/etc/systemd/system").join(format!("{unit_name}.service"))
}

#[cfg(test)]
mod tests {
    //! Needs no root at all: a real SQLite state dir and the test's own
    //! process (a real, genuinely running pid) standing in for a VM's
    //! VMM, so [`ProcessId::is_running`] sees the truth without mocking
    //! it. Exercises the refusal decision `uninstall` makes, independent
    //! of the group/systemd-unit mutation the full CLI path needs root
    //! for (covered instead by `crates/cirro/tests/node_install.rs`).

    use super::*;
    use crate::process::ProcessId;
    use cirro_proto::{EndReason, Ended, VmInfo};

    #[test]
    fn a_unit_argument_with_a_space_or_a_specifier_is_quoted() {
        assert_eq!(unit_arg("/usr/bin/cirro"), "/usr/bin/cirro");
        assert_eq!(
            unit_arg("/srv/my dir/firecracker"),
            "\"/srv/my dir/firecracker\""
        );
        assert_eq!(unit_arg("a%b$c\"d\\e"), "\"a%%b$$c\\\"d\\\\e\"");
    }

    #[test]
    fn a_node_config_from_before_group_created_does_not_remove_the_group() {
        let old = r#"{"socket":"/run/cirro/agent.sock","group":"cirro","subnet":"10.77.0.0/24","unit_name":"cirro-node","firecracker":"/a","jailer":"/b","kernel":"/c","edge":{"http":null,"https":null,"acme_email":null}}"#;
        let config: NodeConfig = serde_json::from_str(old).unwrap();
        assert!(!config.group_created);
    }

    #[test]
    fn an_override_is_copied_under_the_state_dirs_release_dir() {
        let dir = tempdir();
        let state = dir.join("state");
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        let source = ReleaseBinaries {
            firecracker: src.join("firecracker"),
            jailer: src.join("jailer"),
            kernel: src.join("kernel"),
        };
        for path in [&source.firecracker, &source.jailer, &source.kernel] {
            fs::write(path, b"binary").unwrap();
        }
        let copied = installed_release(&state, &source).unwrap();
        let release = layout::release(&state);
        assert_eq!(copied.firecracker, release.join("firecracker"));
        assert_eq!(copied.jailer, release.join("jailer"));
        assert_eq!(copied.kernel, release.join("kernel"));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&copied.firecracker), 0o755);
        assert_eq!(mode(&copied.jailer), 0o755);
        assert_eq!(mode(&copied.kernel), 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn tempdir() -> PathBuf {
        crate::test_util::tempdir("cirro-install-test")
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
