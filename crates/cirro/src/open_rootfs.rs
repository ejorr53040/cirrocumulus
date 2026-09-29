//! The unprivileged half of the rootfs credential drop (issue #14): the
//! hidden `cirro __open-rootfs` subcommand. By the time this code runs,
//! the Node agent has already dropped it to the caller's uid/gid via
//! `pre_exec` (see `cirro_node::rootfs_open`), so an ordinary `open` here
//! is already subject to that caller's real permissions. It opens the
//! path, refuses anything but a regular file, and hands the fd back to
//! the agent over its inherited stdin with `SCM_RIGHTS`.

use nix::fcntl::{self, FcntlArg, OFlag};
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use nix::unistd::{getgid, getuid};
use std::io::IoSlice;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;

pub(crate) fn run(uid: u32, gid: u32, path: &Path) -> ExitCode {
    // Belt and suspenders: the Node agent's `pre_exec` should already have
    // dropped this process to exactly `uid`/`gid` before it execed into us.
    // If that mechanism ever silently regressed, refusing here -- instead
    // of opening `path` as whoever we actually turned out to be -- is what
    // catches it (#14).
    let (actual_uid, actual_gid) = (getuid().as_raw(), getgid().as_raw());
    if actual_uid != uid || actual_gid != gid {
        eprintln!(
            "refusing to open {}: running as {actual_uid}:{actual_gid}, expected {uid}:{gid}",
            path.display()
        );
        return ExitCode::FAILURE;
    }

    // O_NOFOLLOW: a final symlink component was never part of the path the
    // caller was trusted to name. O_NONBLOCK: opening a FIFO for reading
    // otherwise blocks until a writer connects, which would let a rootfs
    // pointed at a named pipe hang this open before the not-a-regular-file
    // check below ever runs (#14) -- cleared again once that check passes,
    // since a regular file has no business being opened non-blocking.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OFlag::O_NOFOLLOW.bits() | OFlag::O_NONBLOCK.bits())
        .open(path)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!("open {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            eprintln!("{} is not a regular file", path.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("stat {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = fcntl::fcntl(&file, FcntlArg::F_SETFL(OFlag::empty())) {
        eprintln!("clear O_NONBLOCK on {}: {e}", path.display());
        return ExitCode::FAILURE;
    }

    let fds = [file.as_raw_fd()];
    let iov = [IoSlice::new(&[0u8])];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    if let Err(e) = sendmsg::<()>(0, &iov, &cmsg, MsgFlags::empty(), None) {
        eprintln!("send the opened rootfs to the Node agent: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
