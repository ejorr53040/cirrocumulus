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
use std::process::Command;

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
// TUNSETIFF is `readwrite`, not `write_ptr`: the kernel writes the final
// interface name back into `ifr_name`, so it needs a `*mut`, not a `*const`.
nix::ioctl_readwrite_bad!(tunsetiff, 0x4004_54ca, IfReq);
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

/// Creates a persistent tap device owned by `uid`, via `sudo ip tuntap
/// add ... user <uid>`, per RESEARCH.md M3's jailer+networking slice.
///
/// `create_persistent_tap` above needs `CAP_NET_ADMIN` in the *caller's*
/// netns (fine under `unshare --net --user --map-root-user`, but that
/// creates an isolated netns a `jailer`-spawned VM never sees, since
/// `Jail::spawn` doesn't pass `--netns` and so shares whichever netns
/// `sudo jailer` itself ran in -- the host's default one). This goes
/// through a narrowly `sudo`-scoped `ip` invocation instead, so an
/// unprivileged caller can create a tap directly in the host's default
/// netns and hand it off, pre-owned, to the uid `jailer` drops the VM's
/// Firecracker process to -- letting that already-privilege-dropped
/// process open and attach the tap without needing `CAP_NET_ADMIN`
/// itself.
pub fn create_persistent_tap_owned_by(name: &str, uid: u32) -> io::Result<()> {
    run_sudo_ip(&[
        "tuntap",
        "add",
        "dev",
        name,
        "mode",
        "tap",
        "user",
        &uid.to_string(),
    ])
}

/// Deletes a tap device created by `create_persistent_tap_owned_by`, via
/// the matching `sudo`-scoped `ip tuntap del`. Best-effort: callers
/// should ignore the error on cleanup, same tolerance this project
/// already gives stale `jailer` jail dirs (see `jailer.rs`) -- a leftover
/// tap is harmless and the next run picks a fresh name. In particular,
/// deleting while the VM that was attached to it is still running fails
/// with `EBUSY` (the kernel won't drop a tap out from under an open fd) --
/// expected and fine to ignore, not a sign anything upstream went wrong.
pub fn delete_persistent_tap(name: &str) -> io::Result<()> {
    run_sudo_ip(&["tuntap", "del", "dev", name, "mode", "tap"])
}

/// Same job as `configure_link` below (assign an address, bring the link
/// up), but via the same narrowly `sudo`-scoped `ip` invocations as
/// `create_persistent_tap_owned_by`, for the same reason: assigning an
/// address (`RTM_NEWADDR`) and bringing a link up (`RTM_SETLINK`) need
/// `CAP_NET_ADMIN` in the caller's netns regardless of who owns the
/// underlying tap device, and an unprivileged caller sharing the host's
/// default netns with a `jailer`-spawned VM doesn't have it.
pub fn configure_link_via_sudo(name: &str, address: Ipv4Addr, prefix_len: u8) -> io::Result<()> {
    run_sudo_ip(&[
        "addr",
        "add",
        &format!("{address}/{prefix_len}"),
        "dev",
        name,
    ])?;
    run_sudo_ip(&["link", "set", name, "up"])
}

/// Enables IPv4 masquerade NAT for `subnet`/`prefix_len` out through
/// `egress_iface`, per RESEARCH.md M3's nftables NAT slice. Without this,
/// a VM's tap-assigned address (configured by `configure_link_via_sudo`)
/// has a host-side gateway but no route out: the kernel won't forward
/// between netns-mates by default, and even with forwarding on, the
/// VM's private source address isn't reachable from the wider internet
/// without being rewritten to the host's own egress address.
///
/// Also adds an explicit `accept` at the `forward` hook for `subnet`, at
/// priority -1 (ahead of the default filter priority most firewall
/// managers use, e.g. `ufw`'s own forward-drop chain at priority 0):
/// masquerading alone isn't enough on a host that already has a
/// default-deny forward policy from something like `ufw` or `firewalld`,
/// since that runs as an independent base chain at the same `forward`
/// hook and can drop the packet before it ever reaches this table's own
/// `postrouting` chain. Cirrocumulus owns the security boundary for its
/// own tap traffic (jailer + cgroups, not the host firewall -- see
/// RESEARCH.md's "Security model"), so explicitly accepting forward
/// traffic for a VM's own private /30 is intentional, not a hole: it
/// only ever un-blocks traffic to/from addresses this project itself
/// assigned.
///
/// Same narrowly `sudo`-scoped-command pattern as
/// `create_persistent_tap_owned_by`/`configure_link_via_sudo`, for the
/// same reason: an unprivileged caller has no `CAP_NET_ADMIN` to flip
/// `ip_forward` or add nftables rules itself. Needs
/// `/etc/sudoers.d/cirro-nft` (see this function's test's module doc for
/// the exact NOPASSWD rule).
///
/// Safe to call more than once: `nft add table`/`add chain` are no-ops
/// if they already exist, and a repeat `add rule` for the same subnet
/// only adds harmless duplicate rules -- the same tolerance this project
/// already gives a re-created tap of the same name.
///
/// This is *host-persistent* state: the `cirro-nat` table (and the broad
/// `forward ... accept` rules it puts at priority -1, ahead of the host
/// firewall) outlive the VM, this process, and -- because those duplicate
/// rules accumulate on every call -- pile up across runs. So every caller
/// that enables NAT must pair it with [`disable_nat`] on teardown (a
/// delete-on-drop guard, so a panic still tears it down), or it leaves a
/// networking artifact behind that affects the host and the next run.
pub fn enable_nat(subnet: Ipv4Addr, prefix_len: u8, egress_iface: &str) -> io::Result<()> {
    let cidr = format!("{subnet}/{prefix_len}");

    run_sudo(&["/usr/bin/sysctl", "-w", "net.ipv4.ip_forward=1"])?;
    run_sudo(&["/usr/bin/nft", "add", "table", "ip", "cirro-nat"])?;
    run_sudo(&[
        "/usr/bin/nft",
        "add",
        "chain",
        "ip",
        "cirro-nat",
        "postrouting",
        "{",
        "type",
        "nat",
        "hook",
        "postrouting",
        "priority",
        "100",
        ";",
        "}",
    ])?;
    run_sudo(&[
        "/usr/bin/nft",
        "add",
        "rule",
        "ip",
        "cirro-nat",
        "postrouting",
        "ip",
        "saddr",
        &cidr,
        "oifname",
        egress_iface,
        "masquerade",
    ])?;
    run_sudo(&[
        "/usr/bin/nft",
        "add",
        "chain",
        "ip",
        "cirro-nat",
        "forward",
        "{",
        "type",
        "filter",
        "hook",
        "forward",
        "priority",
        "-1",
        ";",
        "}",
    ])?;
    run_sudo(&[
        "/usr/bin/nft",
        "add",
        "rule",
        "ip",
        "cirro-nat",
        "forward",
        "ip",
        "saddr",
        &cidr,
        "accept",
    ])?;
    run_sudo(&[
        "/usr/bin/nft",
        "add",
        "rule",
        "ip",
        "cirro-nat",
        "forward",
        "ip",
        "daddr",
        &cidr,
        "accept",
    ])
}

/// Tears down everything [`enable_nat`] added, so a run leaves no NAT
/// artifact behind on the host. Deleting the `cirro-nat` table removes its
/// `postrouting`/`forward` chains and every rule in them in one atomic
/// operation -- including the accumulated duplicates repeated `enable_nat`
/// calls leave -- so this is the whole teardown, not a per-rule one.
///
/// Best-effort and idempotent, matching how the rest of this module treats
/// cleanup (see [`delete_persistent_tap`]): callers should `let _ =` the
/// result on a cleanup path. Deleting a table that isn't there fails with
/// `No such file or directory`, which is folded away here so calling this
/// when nothing was ever set up (or twice) isn't an error -- only a real
/// failure to remove an *existing* table surfaces.
///
/// `net.ipv4.ip_forward` is deliberately left as `enable_nat` set it, not
/// forced back to `0`: it's a single global toggle, commonly already on
/// (Docker, libvirt, a router host), and this code can't know its value
/// before `enable_nat` ran -- so blindly zeroing it on teardown would
/// itself be an artifact, breaking unrelated host forwarding. The
/// `cirro-nat` table is the only state this module owns outright, so it's
/// the only state it removes.
///
/// Uses the same narrowly `sudo`-scoped pattern as `enable_nat`; needs the
/// matching `nft delete table ip cirro-nat` NOPASSWD line (see the NAT
/// test's module doc).
pub fn disable_nat() -> io::Result<()> {
    match run_sudo(&["/usr/bin/nft", "delete", "table", "ip", "cirro-nat"]) {
        Err(e) if e.to_string().contains("No such file or directory") => Ok(()),
        other => other,
    }
}

/// Runs `sudo -n <argv[0]> <argv[1..]>`, capturing (rather than
/// inheriting) the child's stdout/stderr and folding stderr into the
/// returned error on failure, same as `run_sudo_ip` below.
fn run_sudo(argv: &[&str]) -> io::Result<()> {
    let (bin, args) = argv.split_first().expect("argv must be non-empty");
    let output = Command::new("sudo")
        .arg("-n")
        .arg(bin)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "sudo {} failed: {}: {}",
            argv.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Runs `sudo -n /usr/bin/ip <args>`, matching the `/etc/sudoers.d/cirro-tap`
/// NOPASSWD scoping this project's networking tests document. Captures
/// (rather than inherits) the child's stdout/stderr, folding stderr into
/// the returned error on failure -- so a caller that only cares whether it
/// worked (e.g. best-effort cleanup) doesn't leak raw `ip` CLI noise into
/// its own output, while one that does care still gets the real reason.
fn run_sudo_ip(args: &[&str]) -> io::Result<()> {
    let output = Command::new("sudo")
        .args(["-n", "/usr/bin/ip"])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "sudo ip {} failed: {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
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
