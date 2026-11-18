use std::net::Ipv4Addr;
use std::str::FromStr;

/// A Node subnet such as `10.77.0.0/24`. `.1` is the Node's own address;
/// VM addresses are handed out from `.2` up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    network: u32,
    prefix_len: u8,
}

impl FromStr for Subnet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, len) = s
            .split_once('/')
            .ok_or_else(|| format!("{s:?} is not a CIDR like 10.77.0.0/24"))?;
        let addr: Ipv4Addr = addr.parse().map_err(|e| format!("{s:?}: {e}"))?;
        let prefix_len: u8 = len.parse().map_err(|e| format!("{s:?}: {e}"))?;
        if !(16..=30).contains(&prefix_len) {
            return Err(format!("{s:?}: prefix length must be between 16 and 30"));
        }
        let mask = u32::MAX << (32 - prefix_len);
        Ok(Subnet {
            network: u32::from(addr) & mask,
            prefix_len,
        })
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.network), self.prefix_len)
    }
}

impl Subnet {
    pub(crate) fn node_address(self) -> Ipv4Addr {
        Ipv4Addr::from(self.network + 1)
    }

    /// The VM address of this Node's subnet whose low 16 bits are `low`, the
    /// part [`crate::vm::host_id`] names host objects by.
    pub(crate) fn vm_address_with_low16(self, low: u16) -> Option<Ipv4Addr> {
        let address = self.network & 0xFFFF_0000 | u32::from(low);
        let broadcast = self.network | (u32::MAX >> self.prefix_len);
        (self.network + 2..broadcast)
            .contains(&address)
            .then(|| Ipv4Addr::from(address))
    }

    /// Every address a VM may hold, lowest first.
    pub(crate) fn vm_addresses(self) -> impl Iterator<Item = Ipv4Addr> {
        let broadcast = self.network | (u32::MAX >> self.prefix_len);
        (self.network + 2..broadcast).map(Ipv4Addr::from)
    }

    /// Whether an agent on `other`, another Node subnet, would give one of
    /// its VMs a host id this subnet's VMs can have. Host objects are named
    /// by host id alone, so the two agents would tear down each other's VMs.
    pub(crate) fn shares_host_ids_with(self, other: Subnet) -> bool {
        let (ours, theirs) = (self.host_ids(), other.host_ids());
        self != other && ours.start() <= theirs.end() && theirs.start() <= ours.end()
    }

    /// The low 16 bits of this subnet's VM addresses, which
    /// [`crate::vm::host_id`] names host objects by. A prefix of at least 16
    /// keeps them in order, with no wrap.
    fn host_ids(self) -> std::ops::RangeInclusive<u16> {
        let broadcast = self.network | (u32::MAX >> self.prefix_len);
        (self.network + 2) as u16..=(broadcast - 1) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subnet(s: &str) -> Subnet {
        s.parse().unwrap()
    }

    #[test]
    fn subnets_whose_vm_addresses_share_low_16_bits_share_host_ids() {
        let node = subnet("10.77.5.0/24");
        assert!(node.shares_host_ids_with(subnet("10.78.5.0/24")));
        assert!(node.shares_host_ids_with(subnet("10.78.5.128/25")));
        assert!(node.shares_host_ids_with(subnet("10.78.4.0/23")));
        assert!(node.shares_host_ids_with(subnet("10.77.0.0/16")));
    }

    #[test]
    fn a_subnet_shares_no_host_ids_with_itself_or_with_other_low_16_bits() {
        let node = subnet("10.77.5.0/24");
        assert!(!node.shares_host_ids_with(node));
        assert!(!node.shares_host_ids_with(subnet("10.77.6.0/24")));
        assert!(!node.shares_host_ids_with(subnet("10.78.6.0/24")));
        // `.0`, `.1` and the broadcast address are no VM's.
        assert!(!subnet("10.77.5.0/30").shares_host_ids_with(subnet("10.78.5.4/30")));
    }
}
