//! The Node-wide egress policy: what VMs may reach beyond their own VM
//! address. VMs reach the internet through masquerade out of the Node's
//! egress interface, but can't send SMTP (TCP/25), reach other VMs, reach
//! the Node's LAN, or open connections to the Node itself.
//!
//! The rules below key on the source address, so the first rule in each
//! chain drops anything from a VM's veth whose source isn't a VM address: a
//! guest can send from any address it likes, and the Node's rp_filter is
//! loose on many distros.
//!
//! The policy is IPv4's. Guests are given no IPv6 address, so a second
//! table, `ip6 cirro`, drops all IPv6 from VMs' veths.
//!
//! It lives in two nftables tables, `ip cirro` and `ip6 cirro`, which the
//! Node agent ensures at startup and never removes; only `cirro node
//! uninstall` does (#11). Every rule matches only Cirrocumulus interfaces
//! (`cirro-*`, the host ends of VM veths) and the Node subnets in the
//! `ip cirro` table's `node_subnets` set, so the tables never touch other
//! host traffic. That set keeps every Node subnet an agent has run on here,
//! which is how a starting agent finds another Node's subnet whose VMs would
//! share host ids with its own.
//!
//! The tables only ever *drop*. A drop is final across nftables tables,
//! but an accept is not: it only ends the chain it's in, and a default-deny
//! host firewall (ufw, firewalld) still drops the packet in its own chain.
//! Letting VM traffic past such a firewall is therefore the firewall's own
//! configuration, which `cirro node install` sets up (#11).
//!
//! Turning on `net.ipv4.ip_forward` makes the Node forward between all its
//! interfaces, not just VMs' veths. The forward chain still doesn't drop
//! forwarding that neither starts nor ends at a `cirro-*` interface: that
//! drop would be final too, and would cut off every other router on the
//! host (Docker's bridges, libvirt's, a VPN gateway), including ones the
//! operator set up before Cirrocumulus. Keeping the Node from routing
//! between its own LAN and VPN is the host firewall's job, as the
//! networking docs say.

use crate::cmd;
use crate::subnet::Subnet;
use crate::vm::HOST_ID_PREFIX;
use std::io;
use std::path::Path;
use std::process::Command;

/// Where the kernel keeps its IPv6 settings, when it has IPv6 at all.
pub(crate) const IPV6_SYSCTLS: &str = "/proc/sys/net/ipv6";

/// The table's name, in the `ip` family, and in `ip6` for the one rule
/// there.
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
pub(crate) fn ensure_node_policy(subnet: &str, egress_iface: Option<&str>) -> io::Result<()> {
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
         add rule ip {TABLE} forward iifname \"{HOST_ID_PREFIX}*\" ip saddr != @node_subnets drop\n\
         add rule ip {TABLE} forward iifname \"{HOST_ID_PREFIX}*\" ip saddr @node_subnets tcp dport 25 drop\n\
         add rule ip {TABLE} forward iifname \"{HOST_ID_PREFIX}*\" ip saddr @node_subnets ip daddr @node_subnets drop\n\
         add rule ip {TABLE} forward iifname \"{HOST_ID_PREFIX}*\" ip saddr @node_subnets ip daddr @lan drop\n\
         add rule ip {TABLE} input iifname \"{HOST_ID_PREFIX}*\" ip saddr != @node_subnets drop\n\
         add rule ip {TABLE} input iifname \"{HOST_ID_PREFIX}*\" ip saddr @node_subnets ct state new drop\n\
         add element ip {TABLE} node_subnets {{ {subnet} }}\n"
    );
    if let Some(iface) = egress_iface {
        script.push_str(&format!(
            "add element ip {TABLE} egress_ifaces {{ \"{iface}\" }}\n"
        ));
    }
    // The policy above is IPv4's, and guests are given no IPv6, so the Node
    // takes none from a VM's veth. `vm` also turns IPv6 off on each veth,
    // which a later write to `net.ipv6.conf.all.disable_ipv6` would undo.
    if Path::new(IPV6_SYSCTLS).exists() {
        script.push_str(&format!(
            "table ip6 {TABLE} {{\n\
             \tchain forward {{ type filter hook forward priority filter; policy accept; }}\n\
             \tchain input {{ type filter hook input priority filter; policy accept; }}\n\
             }}\n\
             flush chain ip6 {TABLE} forward\n\
             flush chain ip6 {TABLE} input\n\
             add rule ip6 {TABLE} forward iifname \"{HOST_ID_PREFIX}*\" drop\n\
             add rule ip6 {TABLE} input iifname \"{HOST_ID_PREFIX}*\" drop\n"
        ));
    }
    nft(&["-f", "-"], Some(&script))
}

/// Removes the whole egress policy. For `cirro node uninstall`; the agent
/// itself never calls this. Removing a policy that isn't there is fine.
pub(crate) fn remove_node_policy() -> io::Result<()> {
    for family in ["ip", "ip6"] {
        match nft(&["delete", "table", family, TABLE], None) {
            Err(e) if !e.to_string().contains("No such file or directory") => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// The Node subnets in the policy's `node_subnets` set: every one an agent
/// has run on here, running now or not. None before the first agent.
pub(crate) fn node_subnets() -> io::Result<Vec<Subnet>> {
    let listing = match cmd::run(
        Command::new("nft").args(["list", "set", "ip", TABLE, "node_subnets"]),
        None,
    ) {
        Ok(listing) => listing,
        Err(e) if e.to_string().contains("No such file or directory") => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    parse_node_subnets(&String::from_utf8_lossy(&listing))
}

/// The elements of an `nft list set` of `node_subnets`. Only agents add to
/// the set, always a CIDR; anything else was added by hand, and is refused
/// rather than skipped, since it may hold a Node's VMs.
fn parse_node_subnets(listing: &str) -> io::Result<Vec<Subnet>> {
    let Some((_, elements)) = listing.split_once("elements = {") else {
        return Ok(Vec::new());
    };
    let elements = elements.split_once('}').map_or(elements, |(e, _)| e);
    elements
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            e.parse()
                .map_err(|err| io::Error::other(format!("the node_subnets set holds {err}")))
        })
        .collect()
}

const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

/// Turns on IPv4 forwarding, first recording its value at `record` if
/// nothing has been recorded there yet, so `cirro node uninstall` can put
/// back what the Node had before Cirrocumulus.
pub(crate) fn enable_ip_forward(record: &Path) -> io::Result<()> {
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
pub(crate) fn restore_ip_forward(record: &Path) -> io::Result<()> {
    let before = match std::fs::read_to_string(record) {
        Ok(value) => value,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    std::fs::write(IP_FORWARD, before.trim())?;
    std::fs::remove_file(record)
}

/// The interface the Node's IPv4 default route goes out of, if it has one.
pub(crate) fn default_route_iface() -> Option<String> {
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

fn nft(args: &[&str], stdin: Option<&str>) -> io::Result<()> {
    cmd::run(Command::new("nft").args(args), stdin).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_subnets_are_read_from_a_listing_that_wraps() {
        let listing = "table ip cirro {\n\
                       \tset node_subnets {\n\
                       \t\ttype ipv4_addr\n\
                       \t\tflags interval\n\
                       \t\telements = { 10.77.0.0/24, 10.77.180.0/24,\n\
                       \t\t\t     10.78.5.0/25 }\n\
                       \t}\n\
                       }\n";
        let subnets: Vec<String> = parse_node_subnets(listing)
            .unwrap()
            .iter()
            .map(Subnet::to_string)
            .collect();
        assert_eq!(subnets, ["10.77.0.0/24", "10.77.180.0/24", "10.78.5.0/25"]);
    }

    #[test]
    fn an_empty_node_subnets_set_has_no_subnets() {
        let listing = "table ip cirro {\n\tset node_subnets {\n\t\ttype ipv4_addr\n\t\tflags interval\n\t}\n}\n";
        assert!(parse_node_subnets(listing).unwrap().is_empty());
    }

    #[test]
    fn a_node_subnets_element_that_is_no_node_subnet_is_refused() {
        let listing = "\t\telements = { 10.77.0.0/24, 192.168.1.7 }\n";
        let err = parse_node_subnets(listing).unwrap_err().to_string();
        assert!(err.contains("192.168.1.7"), "{err}");
    }
}
