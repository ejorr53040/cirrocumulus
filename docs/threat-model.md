# Threat model

What Cirrocumulus defends against on a single Node (v0.1), what it trusts, and
what it does not try to stop. Report a hole in any of this through
[SECURITY.md](https://github.com/ejorr53040/cirrocumulus/blob/main/SECURITY.md).

## Who is who

| Party | Trusted with | Reaches the Node through |
| --- | --- | --- |
| **Host root / the operator** | Everything. Out of scope as an attacker. | The host itself |
| **`cirro` group member** | Every VM on the Node: start, stop, park, wake, read console logs, claim hostnames. *Not* host root. | The agent's Unix socket (`root:cirro`, `0660`) |
| **App code (the guest)** | Its own VM only | Its VM's virtio devices, its network namespace, vsock |
| **Internet clients** | Nothing | The edge (HTTP/HTTPS) |

A `cirro` group member is trusted the way a `docker` group member is trusted
with a Docker daemon, with one difference: Cirrocumulus means for that trust to
stop at the VMs. The agent opens every rootfs as the caller's own uid and gid,
so the root agent can't be made to read a file the caller couldn't (#14), and
it validates every request itself instead of trusting the CLI.

## Layers between a guest and the host

| Layer | What it does | What it stops |
| --- | --- | --- |
| KVM | A separate guest kernel per workload | Guest-kernel exploits reaching other tenants |
| Firecracker | A minimal VMM: virtio block, net, vsock and a serial console | Most VMM-escape paths that affect QEMU-class VMMs |
| Firecracker seccomp filters | Restrict the VMM process's syscalls | A VMM escape getting a useful host syscall surface |
| jailer | chroot, mount and PID namespaces, a per-VM unprivileged uid | An escaped VMM seeing the host filesystem or other VMs |
| Per-VM network namespace (ADR 0001) | The tap lives in the VM's own namespace, joined to the Node by a veth | An escaped VMM sniffing or spoofing another VM's raw traffic |
| cgroup v2 | CPU, memory and I/O limits per VM | Noisy neighbours, memory exhaustion of the Node |
| nftables egress policy (`ip cirro`) | Masquerade out; drop SMTP, VM-to-VM and LAN destinations | Spam, lateral movement to the LAN, the host, or other VMs |
| VMGenID | The guest kernel reseeds its RNG when a snapshot is restored | Two wakes of one snapshot sharing RNG output |

## The edge

The edge listens where the operator says (`--http`, `--https`) and sends each
request to the App whose Route names its hostname. A client can make it:

- **wake a parked App**, which costs the Node that App's memory until it idles
  again. A client can keep an App awake by requesting it.
- **reach any App by hostname.** Routes are public by design; an App that
  needs authentication does its own.

A TLS connection is for the hostname the client named in its handshake (SNI),
and a request on it for another hostname gets a 421, so one App's certificate
can't be borrowed for another's.

**The Node CA** signs certificates for hostnames that aren't public. Its key
stays root-only in the agent's state dir. It has no name constraints: whoever
can run an App picks its hostname, so a client that trusts the Node CA
(`cirro node ca`) trusts the Node's operators for *any* name. Trust it only on
machines that would.

**ACME** (`--acme-email`, ADR 0008) gets Public hostnames' certificates from
Let's Encrypt or another ACME CA. The agent then calls out to the CA's
directory, keeps an ACME account key and each hostname's certificate key
root-only in its state dir, and answers `/.well-known/acme-challenge/` on the
HTTP edge for any Host while an order is under way. A `cirro` group member can
make the Node order certificates for any Public hostname that points at it.

## Snapshots and state

A parked VM's snapshot holds its guest memory, so it sits in a root-only
directory (`0700`) under the state dir. Snapshots are not encrypted at rest:
anyone with root or the raw disk reads guests' memory. Snapshots and the state
database aren't fsynced (ADR 0006), so a power cut can lose a parked VM.

## Out of scope

- CPU side channels on hosts that don't follow Firecracker's
  [production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)
  (SMT, KSM, swap and microcode settings)
- Host-kernel and KVM 0-days
- Physical access, and a malicious operator or host root
- Denial of service that stays within a VM's own limits; per-tenant quotas
  arrive in M11
- Rate limiting at the edge (M11)

## What has been checked

A manual source review on 2026-09-28 found one High (the agent would boot
any host path as a rootfs; it now opens the rootfs as the caller, #14), three
Mediums (resource bounds checked only in the CLI, an unverified guest kernel
download, an unsigned demo package install), all fixed, and two Lows still open
(the development sudoers rule's breadth, and `jailer::Jail`, which only
the step 0 tests use, with its own `sudo` call; the other unused module is
gone). The OCI layer parser and guest-init's config parser have
been fuzzed for minutes, not days ([fuzz/README.md](https://github.com/ejorr53040/cirrocumulus/blob/main/fuzz/README.md)),
with nothing found. No third party has audited Cirrocumulus. `cargo deny` checks every
dependency against the RustSec advisory database in CI.
