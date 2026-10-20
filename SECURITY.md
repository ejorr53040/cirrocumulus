# Security policy

Cirrocumulus is pre-1.0 software that runs untrusted code in microVMs as
root. The [threat model](docs/threat-model.md) says what it defends against,
what it trusts, and what it doesn't try to stop.

## Reporting a vulnerability

Report privately through GitHub: the repository's **Security** tab, then
**Report a vulnerability**. Please don't open a public issue, discussion or
pull request for a vulnerability.

Include what an attacker needs (a `cirro` group member, code in a guest, a
client of the edge, ...), what they get, and the steps or code to reproduce
it, with the `cirro --version` and host kernel you saw it on.

You will hear back within 72 hours. A confirmed vulnerability gets a fix or
a plan with a date within 14 days, and a GitHub security advisory credited to
you (unless you'd rather not be named) when the fix is released.

## Supported versions

Only the latest release gets security fixes until 1.0.

## Scope

In scope: anything that lets a guest, a client of the edge, or a `cirro`
group member do more than the threat model allows: escape a VM, reach
another VM or the host, read another App's traffic or snapshot, or turn
`cirro` group membership into host root.

Out of scope: the limits the threat model lists (host-kernel 0-days, CPU
side channels on hosts that don't follow Firecracker's production host
setup, physical access, a malicious operator), and vulnerabilities in
Firecracker or jailer themselves, which go to
[Firecracker's security policy](https://github.com/firecracker-microvm/firecracker/blob/main/SECURITY.md).
