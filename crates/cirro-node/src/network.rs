//! Tap device creation and IP configuration for a VM's network interface,
//! per RESEARCH.md M3.
//!
//! Tap *creation* uses the traditional `TUNSETIFF`/`TUNSETPERSIST` ioctls
//! on `/dev/net/tun` -- what `ip tuntap add dev <name> mode tap` does under
//! the hood -- rather than `rtnetlink`: the `rtnetlink`/`netlink-packet-route`
//! crates in use here (0.23/0.33) only model the `tun` link kind as an
//! opaque, attribute-less NLA (`InfoTun::Other`), with none of TUNSETIFF's
//! real fields (type, no-pi, persist, owner), so there's no supported way
//! to create a *working* tap device through them yet. Address assignment
//! and bringing the link up, in contrast, are solid mainstream `rtnetlink`
//! operations, so those go through it normally.

use futures_util::TryStreamExt;
use rtnetlink::{Handle, LinkUnspec};
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;

const IFNAMSIZ: usize = 16;
const IFF_TAP: i16 = 0x0002;
const IFF_NO_PI: i16 = 0x1000;

/// Linux's `struct ifreq`, just enough of it for `TUNSETIFF`. Only
/// `ifr_name` and `ifr_flags` are meaningful for this call -- the rest is
/// padding matching the real struct's size, which is what actually matters
/// for safety here: the kernel reads/writes exactly `size_of::<IfReq>()`
/// bytes through our pointer, so this has to be the real, decades-stable
/// ABI size (16-byte name + a 24-byte union) on Linux/x86_64, not just
/// "big enough".
#[repr(C)]
struct IfReq {
    ifr_name: [u8; IFNAMSIZ],
    ifr_flags: i16,
    _padding: [u8; 22],
}

const _: () = assert!(std::mem::size_of::<IfReq>() == 40);

// Both ioctls are "bad" in nix's sense: their encoded numbers (from
// `_IOW('T', 202, int)` / `_IOW('T', 237, int)` in `linux/if_tun.h`) don't
// match what's actually passed (a `struct ifreq*` for the first; a plain
// `int` is right for the second, so `ioctl_write_int_bad!` fits it exactly).
nix::ioctl_write_ptr_bad!(tunsetiff, 0x4004_54ca, IfReq);
nix::ioctl_write_int_bad!(tunsetpersist, 0x4004_54cb);

/// Creates a persistent tap device with the given name. Persistent means
/// it survives this function's own fd closing -- Firecracker opens it by
/// name later, and doesn't need this process to stay alive or hold it open
/// in the meantime, the same as a tap device created by `ip tuntap add`.
pub fn create_persistent_tap(name: &str) -> io::Result<()> {
    assert!(
        name.len() < IFNAMSIZ,
        "interface name {name:?} too long for IFNAMSIZ ({IFNAMSIZ})"
    );

    let fd = nix::fcntl::open(
        "/dev/net/tun",
        nix::fcntl::OFlag::O_RDWR,
        nix::sys::stat::Mode::empty(),
    )?;

    let mut ifr_name = [0u8; IFNAMSIZ];
    ifr_name[..name.len()].copy_from_slice(name.as_bytes());
    let mut req = IfReq {
        ifr_name,
        ifr_flags: IFF_TAP | IFF_NO_PI,
        _padding: [0; 22],
    };

    // SAFETY: `fd` is a freshly opened, valid, still-open file descriptor;
    // `req` is a validly initialized, correctly sized `ifreq` for the
    // duration of this call.
    unsafe { tunsetiff(fd.as_raw_fd(), &mut req) }?;
    // SAFETY: TUNSETPERSIST takes a plain int (1 = persist), no pointer
    // into memory we need to keep alive or sized correctly.
    unsafe { tunsetpersist(fd.as_raw_fd(), 1) }?;

    Ok(())
}

/// Assigns an IPv4 address (with prefix length, e.g. `30` for a `/30`) to
/// an existing link and brings it up -- ordinary `rtnetlink` operations,
/// the same as `ip addr add <addr>/<prefix> dev <name>` + `ip link set
/// <name> up`.
pub async fn configure_link(
    handle: &Handle,
    name: &str,
    address: Ipv4Addr,
    prefix_len: u8,
) -> Result<(), rtnetlink::Error> {
    let link = handle
        .link()
        .get()
        .match_name(name.to_string())
        .execute()
        .try_next()
        .await?
        .unwrap_or_else(|| panic!("no such link: {name}"));
    let index = link.header.index;

    handle
        .address()
        .add(index, std::net::IpAddr::V4(address), prefix_len)
        .execute()
        .await?;

    handle
        .link()
        .set(LinkUnspec::new_with_index(index).up().build())
        .execute()
        .await?;

    Ok(())
}
