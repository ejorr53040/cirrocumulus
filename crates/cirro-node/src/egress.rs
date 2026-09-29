//! The Node-wide egress policy: what VMs may reach beyond their own VM
//! address. VMs reach the internet through masquerade out of the Node's
//! egress interface, but can't send SMTP (TCP/25), reach other VMs, reach
//! the Node's LAN, or open connections to the Node itself.
//!
//! It all lives in one nftables table, `ip cirro`, which the Node agent
//! ensures at startup and never removes; only `cirro node uninstall` does
//! (#11). Every rule matches only Cirrocumulus interfaces (`cirro-*`, the
//! host ends of VM veths) and the Node subnets in the table's
//! `node_subnets` set, so the table never touches other host traffic.
//!
//! The table only ever *drops*. A drop is final across nftables tables,
//! but an accept is not: it only ends the chain it's in, and a default-deny
//! host firewall (ufw, firewalld) still drops the packet in its own chain.
//! Letting VM traffic past such a firewall is therefore the firewall's own
//! configuration, which `cirro node install` sets up (#11).

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// The table's name, in the `ip` family.
const TABLE: &str = "cirro";

/// Destinations VMs may not open connections to: private, carrier-grade
/// NAT and link-local ranges, which covers the Node's LAN, other VMs and
/// cloud metadata endpoints.
const LAN_RANGES: &str = "10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16";

/// Makes the Node's egress policy cover `subnet` (a CIDR), masquerading it
/// out of `egress_iface` when there is one. Safe to call again, from any
/// number of agents: the whole update is one nftables transaction that
/// declares the table (a no-op if it exists), rewrites its rules from
/// scratch and adds to its sets, so it never duplicates a rule and never
/// drops another agent's subnet.
pub fn ensure_node_policy(subnet: &str, egress_iface: Option<&str>) -> io::Result<()> {
    let mut script = format!(
        "table ip {TABLE} {{\n\
         \tset node_subnets {{ type ipv4_addr; flags interval; }}\n\
         \tset egress_ifaces {{ type ifname; }}\n\
         \tset lan {{ type ipv4_addr; flags interval; elements = {{ {LAN_RANGES} }}; }}\n\
         \tchain postrouting {{ type nat hook postrouting priority srcnat; policy accept; }}\n\
         \tchain forward {{ type filter hook forward priority filter; policy accept; }}\n\
         \tchain input {{ type filter hook input priority filter; policy accept; }}\n\
         }}\n\
         flush chain ip {TABLE} postrouting\n\
         flush chain ip {TABLE} forward\n\
         flush chain ip {TABLE} input\n\
         add rule ip {TABLE} postrouting ip saddr @node_subnets oifname @egress_ifaces masquerade\n\
         add rule ip {TABLE} forward iifname \"cirro-*\" ip saddr @node_subnets tcp dport 25 drop\n\
         add rule ip {TABLE} forward iifname \"cirro-*\" ip saddr @node_subnets ip daddr @node_subnets drop\n\
         add rule ip {TABLE} forward iifname \"cirro-*\" ip saddr @node_subnets ip daddr @lan drop\n\
         add rule ip {TABLE} input iifname \"cirro-*\" ip saddr @node_subnets ct state new drop\n\
         add element ip {TABLE} node_subnets {{ {subnet} }}\n"
    );
    if let Some(iface) = egress_iface {
        script.push_str(&format!(
            "add element ip {TABLE} egress_ifaces {{ \"{iface}\" }}\n"
        ));
    }
    nft(&["-f", "-"], Some(&script))
}

/// Removes the whole egress policy. For `cirro node uninstall`; the agent
/// itself never calls this. Removing a policy that isn't there is fine.
pub fn remove_node_policy() -> io::Result<()> {
    match nft(&["delete", "table", "ip", TABLE], None) {
        Err(e) if e.to_string().contains("No such file or directory") => Ok(()),
        other => other,
    }
}

/// Turns on IPv4 forwarding, first recording its value at `record` if
/// nothing has been recorded there yet, so `cirro node uninstall` can put
/// back what the Node had before Cirrocumulus.
pub fn enable_ip_forward(record: &Path) -> io::Result<()> {
    const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
    if !record.exists() {
        let before = std::fs::read_to_string(IP_FORWARD)?;
        std::fs::write(record, before.trim())?;
    }
    std::fs::write(IP_FORWARD, "1")
}

/// Reverses [`enable_ip_forward`]: puts `net.ipv4.ip_forward` back to
/// whatever it recorded at `record`, then removes the record. For `cirro
/// node uninstall`. A missing record means no agent ever ran here (or a
/// previous uninstall already restored it) -- a no-op, not an error, the
/// same tolerance [`remove_node_policy`] gives a missing table.
pub fn restore_ip_forward(record: &Path) -> io::Result<()> {
    const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
    let before = match std::fs::read_to_string(record) {
        Ok(value) => value,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    std::fs::write(IP_FORWARD, before.trim())?;
    std::fs::remove_file(record)
}

/// The interface the Node's IPv4 default route goes out of, if it has one.
pub fn default_route_iface() -> Option<String> {
    let output = Command::new("ip")
        .args(["-o", "-4", "route", "show", "default"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .skip_while(|w| *w != "dev")
        .nth(1)
        .map(str::to_string)
}

/// Runs `nft`, feeding it `stdin` when given, and folds its stderr into the
/// error on failure.
fn nft(args: &[&str], stdin: Option<&str>) -> io::Result<()> {
    let mut child = Command::new("nft")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "nft {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
