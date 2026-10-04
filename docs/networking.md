# Networking Firecracker VMs

How a Cirrocumulus VM is wired, from the guest's `eth0` to the internet,
and why each piece is there.

```text
guest eth0 172.16.0.2 ─ tap0 172.16.0.1 ─┐  VM's network namespace
                                         │  1:1 NAT 172.16.0.2 <-> 10.77.0.2
                                  veth ──┘
                                    │
                            Node: route to 10.77.0.2,
                            `ip cirro` nftables table,
                            masquerade out of the default route
```

## A namespace per VM

Firecracker opens the VM's tap device, so whoever can reach the tap can
read and write the guest's raw frames. Each VM's tap lives in a network
namespace of its own, and jailer starts Firecracker in it (`--netns`), so an
escaped VMM sees one tap and nothing else
([ADR 0001](adr/0001-per-vm-network-namespace.md)). A veth pair joins the
namespace to the Node.

## The same address in every guest

Every guest is `172.16.0.2/30` with gateway `172.16.0.1`, set on the kernel
command line (`ip=`), and the namespace NATs it 1:1 to the VM's own address
from the Node subnet (`10.77.0.0/24` by default). A snapshot captures the
guest's network setup, so with a fixed guest address a parked VM can wake
in a new namespace with a different VM address and need no change inside
([ADR 0003](adr/0003-fixed-guest-address.md)). For the same reason every tap
has the same MAC, `06:00:ac:10:00:01`: a woken guest's ARP cache still
names its gateway correctly, so it can speak first without waiting for an
ARP timeout ([ADR 0005](adr/0005-fixed-tap-mac.md)).

## The egress policy

One Node-wide nftables table, `ip cirro`, applies to every VM. It
masquerades VMs' traffic out of the Node's default route, and drops:

- SMTP (TCP port 25), so a VM can't send spam from the Node's address;
- traffic between VMs, which go through the edge or the internet like any
  other client;
- traffic to private and link-local ranges (`10/8`, `100.64/10`,
  `169.254/16`, `172.16/12`, `192.168/16`), so a VM can't reach the LAN, the
  cloud metadata address, or the Node itself;
- new connections from VMs to the Node.

The table only ever drops, so a host firewall such as ufw still applies on
top. On a host with ufw's default forward policy of DROP, VM traffic also
needs `ufw route allow in on cirro-+ out on <egress interface>`.

## Reaching a VM

From the Node, each VM is at its VM address (`curl http://10.77.0.2/`).
From anywhere else, an App is reached through the edge by hostname: see
[Apps and the edge](https://github.com/ejorr53040/cirrocumulus#apps-and-the-edge).
