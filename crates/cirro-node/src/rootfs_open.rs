//! Opens a rootfs path as the uid/gid of whoever asked the Node agent to
//! run a VM with it (issue #14), so a `cirro` group member can never make
//! the agent -- root -- read a file or device the caller couldn't read
//! themselves.
//!
//! The agent can't just check `access()` and then `open()` as root: that's
//! a TOCTOU race, and `access()` doesn't see what a real open as that
//! uid/gid would (ACLs, read-only bind mounts, and so on). Instead it
//! re-execs itself as a short-lived helper (`cirro __open-rootfs`) that
//! has dropped every privilege but the caller's uid/gid before it execs,
//! opens the path there, and hands the resulting fd back over a
//! socketpair with `SCM_RIGHTS`. Forking the agent's own multithreaded
//! async process directly would be unsound (locks and other threads don't
//! survive a raw `fork`), so this goes through `Command::spawn` with
//! `pre_exec` instead, which only runs simple syscalls before the exec
//! wipes the process image.
//!
//! `SO_PEERCRED` only gives the caller's primary gid, never their
//! supplementary groups, so a rootfs readable only through a supplementary
//! group is (safely) denied here too -- narrower than "open as them", but
//! it never reopens the hole this module exists to close.

use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use nix::unistd::{Gid, Uid, setgroups, setresgid, setresuid};
use std::io::{self, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Why [`open_as`] failed: whether the helper ran and refused `path` itself
/// (the caller's request was bad) or the credential-drop mechanism broke
/// (not the caller's fault), so the agent can answer with the right HTTP
/// status.
pub(crate) enum Error {
    /// The helper ran as the caller and refused the rootfs (permission
    /// denied, not a regular file, an unexpected symlink, ...).
    Denied(String),
    /// Something about spawning or talking to the helper itself failed.
    Failed(String),
}

/// Opens `path` as `uid`/`gid`, refusing anything but a regular file. The
/// error is already fit to show the `cirro run` caller who asked for it.
pub(crate) fn open_as(uid: u32, gid: u32, path: &Path) -> Result<std::fs::File, Error> {
    let (parent_sock, child_sock) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| Error::Failed(format!("open a socketpair: {e}")))?;
    let exe = std::env::current_exe()
        .map_err(|e| Error::Failed(format!("find the cirro binary: {e}")))?;

    let target_uid = Uid::from_raw(uid);
    let target_gid = Gid::from_raw(gid);
    let mut cmd = Command::new(exe);
    cmd.arg("__open-rootfs")
        .arg(uid.to_string())
        .arg(gid.to_string())
        .arg(path)
        .stdin(Stdio::from(OwnedFd::from(child_sock)))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: the closure only calls setgroups/setresgid/setresuid, which
    // are async-signal-safe and touch no locks or allocator state, so
    // running them between fork and exec (the only place `pre_exec` runs)
    // is sound. It drops every privilege the helper could have before it
    // execs, so `cirro __open-rootfs` never runs as anyone but `uid`/`gid`.
    unsafe {
        cmd.pre_exec(move || {
            setgroups(&[]).map_err(io::Error::from)?;
            setresgid(target_gid, target_gid, target_gid).map_err(io::Error::from)?;
            setresuid(target_uid, target_uid, target_uid).map_err(io::Error::from)?;
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| Error::Failed(format!("spawn the rootfs-open helper: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Failed(format!("wait for the rootfs-open helper: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::Denied(if stderr.is_empty() {
            format!("the rootfs-open helper exited with {}", output.status)
        } else {
            stderr
        }));
    }

    let mut buf = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);
    let msg = recvmsg::<()>(
        parent_sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )
    .map_err(|e| Error::Failed(format!("receive the opened rootfs from the helper: {e}")))?;
    let fd = msg
        .cmsgs()
        .map_err(|e| Error::Failed(format!("read the helper's response: {e}")))?
        .find_map(|c| match c {
            ControlMessageOwned::ScmRights(fds) => fds.first().copied(),
            _ => None,
        })
        .ok_or_else(|| Error::Failed("the rootfs-open helper sent no file".to_string()))?;
    // SAFETY: `fd` came from a `ScmRights` control message the helper just
    // sent over `parent_sock`, so it's a valid, open fd nothing else in
    // this process has seen or owns yet.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
