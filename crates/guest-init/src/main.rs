//! `guest-init`: PID 1 inside the Firecracker guest.
//!
//! It mounts the pseudo-filesystems, prints a boot marker, reads the app's
//! config over vsock (so the host supplies it at boot rather than baking it
//! into the image), and forks and execs the app as its configured user.
//! Until no children are left it reaps every one, the app's orphans
//! included, and turns a host-triggered shutdown (Firecracker's
//! `SendCtrlAltDel` action) into a `SIGTERM` for the app, instead of the
//! kernel's default hard reset wiping out the guest before the app gets to
//! react. Then it resets the VM, which Firecracker exits on.

mod launch;

use cirro_guest_init::FORWARDING_MARKER;
use cirro_guest_init::config::{self, Config};
use nix::errno::Errno;
use nix::mount::{MsFlags, mount};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::reboot::{RebootMode, reboot, set_cad_enabled};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept, bind, listen, setsockopt,
    socket, sockopt,
};
use nix::sys::time::{TimeVal, TimeValLike};
use nix::sys::wait::waitpid;
use nix::unistd::{
    ForkResult, Gid, Pid, Uid, User, chdir, execve, fork, initgroups, setgid, setgroups, setuid,
};
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// The tracked app's pid while it runs: 0 before it is forked and after it
/// is reaped, so a late request can't hit a process that reused the pid.
static APP_PID: AtomicI32 = AtomicI32::new(0);

/// A host shutdown request that arrived before the app was forked, which
/// `main` forwards as soon as `APP_PID` is set.
static SHUTDOWN_PENDING: AtomicBool = AtomicBool::new(false);

/// Forwards a host shutdown request to the app as `SIGTERM`, here in the
/// handler rather than from the reap loop: a flag the loop checked only
/// when `waitpid` was interrupted was lost when the signal landed between
/// two `waitpid` calls. Before the app exists the request is kept as
/// `SHUTDOWN_PENDING`; guest-init is single-threaded, so the handler runs
/// whole between two of `main`'s steps and the request can't fall between
/// the flag and `APP_PID`.
extern "C" fn handle_sigint(_signal: nix::libc::c_int) {
    let pid = APP_PID.load(Ordering::SeqCst);
    if pid > 0 {
        forward_shutdown(pid);
    } else {
        SHUTDOWN_PENDING.store(true, Ordering::SeqCst);
    }
}

fn forward_shutdown(pid: nix::libc::pid_t) {
    // SAFETY: `write(2)` and `kill(2)` are async-signal-safe (POSIX
    // 2.4.3), the buffer is static, and an error (the app already gone)
    // leaves nothing to undo.
    unsafe {
        nix::libc::write(
            1,
            FORWARDING_MARKER.as_ptr().cast(),
            FORWARDING_MARKER.len(),
        );
        nix::libc::write(1, c"\n".as_ptr().cast(), 1);
        nix::libc::kill(pid, nix::libc::SIGTERM);
    }
}

/// Printed to the console (ttyS0) once the pseudo-filesystems are mounted,
/// so the boot harness can confirm guest-init reached this point as PID 1.
const BOOT_MARKER: &str = "GUEST_INIT_MOUNTS_OK";

/// Mounts the pseudo-filesystems a guest needs before anything else can
/// run: `/proc` and `/sys` are required (process/kernel introspection);
/// `/dev` (devtmpfs) is best-effort, matching Step 0's own script, since a
/// kernel built without devtmpfs support would otherwise make PID 1 fail
/// outright over a filesystem no one has asked to use yet.
///
/// All three are `nosuid`. `/proc` and `/sys` are also `nodev` and `noexec`,
/// as `docker run` mounts them. `/dev` isn't `nodev`: its device nodes are how
/// the app opens `/dev/null` and the disks.
fn mount_pseudo_filesystems() {
    mount(
        Some("proc"),
        Path::new("/proc"),
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .expect("mount /proc");

    mount(
        Some("sysfs"),
        Path::new("/sys"),
        Some("sysfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .expect("mount /sys");

    let _ = mount(
        Some("devtmpfs"),
        Path::new("/dev"),
        Some("devtmpfs"),
        MsFlags::MS_NOSUID,
        None::<&str>,
    );
}

/// Reads the app config over vsock. Firecracker
/// exposes vsock to the host as a Unix socket at a configured path; a host
/// that connects there and sends `CONNECT <port>\n` gets that connection
/// relayed straight into whatever this guest has accepted on that port,
/// once it has one -- so this binds to `VMADDR_CID_ANY` (accept a
/// connection addressed to any local CID, since the host doesn't need to
/// know or care what CID the guest was assigned) rather than a specific
/// CID.
///
/// Reads until EOF: the host closes its end once the whole config has been
/// written.
fn read_config_from_vsock() -> Result<Config, String> {
    let listen_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| format!("create vsock socket: {e}"))?;

    let addr = VsockAddr::new(nix::libc::VMADDR_CID_ANY, config::CONFIG_PORT);
    bind(listen_fd.as_raw_fd(), &addr).map_err(|e| format!("bind vsock config port: {e}"))?;
    listen(&listen_fd, Backlog::MAXCONN)
        .map_err(|e| format!("listen on vsock config port: {e}"))?;

    wait_readable(&listen_fd)?;
    let conn_fd = accept(listen_fd.as_raw_fd())
        .map_err(|e| format!("accept vsock config connection: {e}"))?;
    // SAFETY: `accept` returns a valid, newly owned fd on success.
    let conn_fd = unsafe { OwnedFd::from_raw_fd(conn_fd) };
    // A sender that connects and then stalls would otherwise hold PID 1 forever.
    let timeout = TimeVal::milliseconds(i64::from(CONFIG_WAIT_MS));
    setsockopt(&conn_fd, sockopt::ReceiveTimeout, &timeout)
        .map_err(|e| format!("set vsock config read timeout: {e}"))?;

    config::read_config(File::from(conn_fd))
}

/// How long guest-init waits for the agent's config connection, and then for
/// each read of it. The agent gives up after 10 s, so this fires only when no
/// agent is coming: a backstop that powers the VM off rather than letting PID
/// 1 wait forever.
const CONFIG_WAIT_MS: u16 = 30_000;

/// Waits until `fd` is readable, for at most `CONFIG_WAIT_MS`. A signal (a host
/// shutdown request, see `handle_sigint`) doesn't end the wait: the request is
/// held for the app, and `main` forwards it once the app is forked.
fn wait_readable(fd: impl AsFd) -> Result<(), String> {
    let timeout = PollTimeout::from(CONFIG_WAIT_MS);
    loop {
        let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Ok(0) => return Err(format!("no config connection within {CONFIG_WAIT_MS} ms")),
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(format!("poll vsock config port: {e}")),
        }
    }
}

/// Forks and execs the configured app in the child.
/// This is `fork`+`execve`, not a bare `execve` of PID 1 itself: guest-init
/// has to stay running as a distinct process to reap zombies, forward
/// signals and power off on the app's exit, none of which is possible if
/// the app's image had replaced guest-init's own.
///
/// Returns the child's pid immediately after forking; reaping (including
/// this child) is `reap_until_no_children`'s job, not this function's.
fn spawn_configured_app(config: &Config) -> Pid {
    // SAFETY: single-threaded at this point in boot (PID 1, right after
    // mount), so the child sees a consistent, not-mid-mutation process
    // image between fork and execve.
    match unsafe { fork() }.expect("fork configured app") {
        ForkResult::Child => {
            // Never returns. A failure is reported on the console and ends
            // the app, which guest-init then sees exit like any other.
            let e = exec_app(config);
            eprintln!("guest-init: execv {}: {e}", config.exec);
            std::process::exit(127);
        }
        ForkResult::Parent { child } => child,
    }
}

/// In the forked child: enters the workdir, drops to the configured user
/// and execs the app with its environment. Returns only on failure.
fn exec_app(config: &Config) -> String {
    let env = launch::app_env(&config.env);
    if let Some(dir) = &config.workdir
        && let Err(e) = chdir(dir.as_str())
    {
        return format!("chdir {dir}: {e}");
    }
    if let Some(user) = config.user
        && let Err(e) = switch_user(user)
    {
        return e;
    }
    let path = launch::lookup(&env, "PATH").expect("app_env always sets PATH");
    let Some(file) = launch::resolve(&config.exec, path, is_executable) else {
        return format!("not found on PATH {path}");
    };
    let to_cstrings = |items: &[String]| -> Result<Vec<CString>, String> {
        items
            .iter()
            .map(|s| CString::new(s.as_str()).map_err(|_| format!("NUL byte in {s:?}")))
            .collect()
    };
    let argv = match to_cstrings(&[std::slice::from_ref(&config.exec), &config.args].concat()) {
        Ok(argv) => argv,
        Err(e) => return e,
    };
    let envp = match to_cstrings(&env) {
        Ok(envp) => envp,
        Err(e) => return e,
    };
    let Ok(file) = CString::new(file.as_os_str().as_encoded_bytes()) else {
        return "NUL byte in the resolved path".to_string();
    };
    match execve(&file, &argv, &envp) {
        Ok(never) => match never {},
        Err(e) => e.to_string(),
    }
}

/// Drops to `user` as `docker run` does: the supplementary groups are the
/// ones the image's `/etc/group` lists for the user's name, plus the primary
/// gid. A uid with no `/etc/passwd` entry has no name to look groups up by,
/// so it keeps only the primary gid; so does one in an image with no
/// `/etc/passwd` or `/etc/group` at all (musl reports a missing file as
/// `ENOENT`, not as no entry).
///
/// No `PR_SET_NO_NEW_PRIVS`: `docker run` doesn't set it either, and the
/// images it runs rely on setuid binaries such as `su`. Guest root isn't a
/// boundary (docs/threat-model.md).
fn switch_user(user: config::User) -> Result<(), String> {
    let uid = Uid::from_raw(user.uid);
    let gid = Gid::from_raw(user.gid);
    let fail = |e: Errno| format!("switch to user {}:{}: {e}", user.uid, user.gid);
    let entry = match User::from_uid(uid) {
        Err(Errno::ENOENT) => None,
        found => found.map_err(fail)?,
    };
    let grouped = match entry {
        Some(entry) => {
            let name = CString::new(entry.name).map_err(|_| "NUL byte in user name".to_string())?;
            initgroups(&name, gid)
        }
        None => Err(Errno::ENOENT),
    };
    match grouped {
        Err(Errno::ENOENT) => setgroups(&[gid]).map_err(fail)?,
        other => other.map_err(fail)?,
    }
    setgid(gid).map_err(fail)?;
    setuid(uid).map_err(fail)
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Makes a host-triggered shutdown request (Firecracker's `SendCtrlAltDel`
/// action) observable to `main`'s reap loop instead of tearing the guest
/// down before any userspace code runs.
///
/// By default Ctrl-Alt-Del is "hard": the kernel resets the machine
/// straight from the keyboard-interrupt handler, a reset Firecracker traps
/// and exits on -- so the VM shuts down, but the app never gets a chance
/// to react. `set_cad_enabled(false)` switches the kernel to "soft" CAD, where
/// the same keypress instead sends `SIGINT` to PID 1 specifically, which
/// this installs a handler for.
fn install_shutdown_request_handler() {
    set_cad_enabled(false).expect("set soft Ctrl-Alt-Del (SIGINT to init)");

    let action = SigAction::new(
        SigHandler::Handler(handle_sigint),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: `handle_sigint` only touches atomics and makes
    // async-signal-safe calls (see `forward_shutdown`).
    unsafe { signal::sigaction(Signal::SIGINT, &action) }.expect("install SIGINT handler");
}

/// Reaps every child until none remain: a grandchild the app forked and
/// didn't wait on is reparented to guest-init (PID 1), so waiting on the
/// app's pid alone would leave it unreaped. `waitpid(-1)` until `ECHILD`
/// sweeps up the app and any such orphan before shutdown runs. Once `app`
/// is reaped its pid is free for reuse, so `APP_PID` goes back to 0.
///
/// `waitpid` returns `EINTR` when `SIGINT` (see `handle_sigint`)
/// interrupts the blocking wait, since `SA_RESTART` isn't set: not an
/// error, just a reason to wait again.
fn reap_until_no_children(app: Pid) {
    loop {
        match waitpid(Pid::from_raw(-1), None) {
            Ok(status) if status.pid() == Some(app) => APP_PID.store(0, Ordering::SeqCst),
            Ok(_) | Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) => return,
            Err(e) => panic!("waitpid(-1): {e}"),
        }
    }
}

/// Shuts the VM down. `reboot(2)` doesn't return on success -- the kernel
/// tears the machine down mid-syscall -- which is what PID 1 needs:
/// *returning* from here (letting `main` end) panics the guest kernel
/// ("Attempted to kill init!") instead of shutting down cleanly.
///
/// Not `RB_POWER_OFF`: Firecracker's minimal device model has no ACPI power
/// button, so the kernel just halts ("Power off not available: System
/// halted instead") and the Firecracker process stays alive forever. A
/// reboot takes the x86 reset path Firecracker traps and exits the VMM on.
fn shutdown() -> ! {
    match reboot(RebootMode::RB_AUTOBOOT) {
        Ok(never) => match never {},
        Err(e) => panic!("reboot RB_AUTOBOOT failed: {e}"),
    }
}

fn main() {
    mount_pseudo_filesystems();
    install_shutdown_request_handler();

    println!("{BOOT_MARKER}");
    // A println isn't enough on its own: the guest kernel's serial driver
    // can still be draining this write into the UART when the next line
    // runs, and PID 1 returning from main is fatal (the kernel panics with
    // "Attempted to kill init!"), tearing down the console mid-write and
    // truncating or dropping the marker entirely. So flush explicitly.
    std::io::stdout().flush().expect("flush boot marker");

    let config = match read_config_from_vsock() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("guest-init: {e}; shutting down");
            shutdown();
        }
    };
    let app_pid = spawn_configured_app(&config);
    APP_PID.store(app_pid.as_raw(), Ordering::SeqCst);
    if SHUTDOWN_PENDING.load(Ordering::SeqCst) {
        forward_shutdown(app_pid.as_raw());
    }
    reap_until_no_children(app_pid);

    shutdown();
}
