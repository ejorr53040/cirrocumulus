//! Spawns a jailed Firecracker instance via `jailer` (chroot + cgroup +
//! privilege drop), per RESEARCH.md M3. Mirrors the flags
//! `scripts/step0/run_jailer.sh` already proved work by hand: `jailer`
//! itself must run as root -- via `sudo`, relying on the NOPASSWD sudoers
//! rule scoped to the exact jailer binary path that `scripts/step0/prereqs.sh`
//! sets up -- but the Firecracker process it `exec`s into drops to the
//! given unprivileged uid/gid.

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for `jailer` to create the jail root directory before
/// giving up. Matches the other real-Firecracker tests' boot timeout --
/// jailer creates it near-instantly once it starts, so this is generous.
const JAIL_ROOT_TIMEOUT: Duration = Duration::from_secs(10);

/// What `jailer` needs to spawn a jailed Firecracker instance.
pub struct JailerConfig {
    /// Unique per-run jail id (`jailer --id`). `jailer` only chowns the
    /// leaf jail dir back to `uid`/`gid`, not the directories above it, so
    /// stale jails from past ids can't be cleaned up without sudo -- a
    /// fresh id per run sidesteps that rather than fighting it.
    pub id: String,
    /// The Firecracker binary `jailer` will `exec` into once it's dropped
    /// privileges.
    pub exec_file: PathBuf,
    pub uid: u32,
    pub gid: u32,
    /// Must be short (the API socket path inside the jail has to fit in a
    /// `sockaddr_un`) and not on a `nodev`-mounted filesystem (`jailer`
    /// mknods `/dev/kvm` inside the chroot, and device nodes are inert on
    /// `nodev`).
    pub chroot_base_dir: PathBuf,
}

/// A running jailed Firecracker instance.
pub struct Jail {
    /// The `sudo jailer` process. Kept alive (rather than detached) so its
    /// lifetime is tied to this struct's, but by the time this exists
    /// `jailer` has already dropped privileges and `exec`'d into
    /// Firecracker -- killing the *tracked* Firecracker process (see
    /// `Drop`) doesn't need sudo even though spawning it did.
    child: Child,
    root: PathBuf,
}

impl Jail {
    /// Spawns `sudo <jailer_bin> --id --exec-file --uid --gid
    /// --chroot-base-dir --cgroup-version 2` and waits for the jail root
    /// directory (`<chroot_base_dir>/<exec_file basename>/<id>/root`) to
    /// appear, the same poll `run_jailer.sh` does by hand. Does not wait
    /// for the API socket inside it -- callers that need it poll for that
    /// themselves, same as the unjailed boot tests do.
    pub async fn spawn(
        jailer_bin: &Path,
        config: &JailerConfig,
        console_log: &std::fs::File,
    ) -> io::Result<Self> {
        std::fs::create_dir_all(&config.chroot_base_dir)?;

        let child = Command::new("sudo")
            .arg(jailer_bin)
            .arg("--id")
            .arg(&config.id)
            .arg("--exec-file")
            .arg(&config.exec_file)
            .arg("--uid")
            .arg(config.uid.to_string())
            .arg("--gid")
            .arg(config.gid.to_string())
            .arg("--chroot-base-dir")
            .arg(&config.chroot_base_dir)
            .arg("--cgroup-version")
            .arg("2")
            .stdout(Stdio::from(console_log.try_clone()?))
            .stderr(Stdio::from(console_log.try_clone()?))
            .spawn()?;

        let root = jail_root(&config.chroot_base_dir, &config.exec_file, &config.id);
        let deadline = Instant::now() + JAIL_ROOT_TIMEOUT;
        while !root.exists() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("jailer never created jail root at {}", root.display()),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(Jail { child, root })
    }

    /// The jail's root directory (what the jailed process sees as `/`).
    /// Resources referenced over the Firecracker API (kernel, rootfs) must
    /// be placed under here first, per docs/jailer.md.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the Firecracker API socket appears once the jailed process is
    /// up, as a host path (i.e. already joined with `root()`).
    pub fn api_socket_path(&self) -> PathBuf {
        self.root.join("run/firecracker.socket")
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        // `jailer` writes the exec'd Firecracker process's pid here, per
        // docs/jailer.md -- the same pid `run_jailer.sh`'s own cleanup
        // trap kills. It's a different pid than `self.child` by this
        // point (that's `sudo`/`jailer`'s own pid, which normally exits on
        // its own once its child does).
        if let Ok(pid_str) = std::fs::read_to_string(self.root.join("firecracker.pid")) {
            if let Ok(raw_pid) = pid_str.trim().parse::<i32>() {
                let _ = kill(Pid::from_raw(raw_pid), Signal::SIGKILL);
            }
        }
        let _ = self.child.wait();
    }
}

fn jail_root(chroot_base_dir: &Path, exec_file: &Path, id: &str) -> PathBuf {
    chroot_base_dir
        .join(
            exec_file
                .file_name()
                .expect("exec_file has a filename"),
        )
        .join(id)
        .join("root")
}
