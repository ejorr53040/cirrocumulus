# Summary

[Introduction](introduction.md)

# Guides

- [A Firecracker rootfs from a Docker image](rootfs-from-an-image.md)
- [Running Firecracker with jailer](firecracker-with-jailer.md)
- [Networking Firecracker VMs](networking.md)
- [Firecracker snapshots: park and wake](park-and-wake.md)
- [Compared with Coolify, Dokku, Proxmox and Fly.io](comparison.md)

# Security

- [Threat model: multi-tenant isolation with microVMs](threat-model.md)

# Design decisions

- [0001 A network namespace per VM](adr/0001-per-vm-network-namespace.md)
- [0002 A privileged Node agent](adr/0002-privileged-node-agent.md)
- [0003 A fixed guest address](adr/0003-fixed-guest-address.md)
- [0004 Images build in the unprivileged CLI](adr/0004-images-build-in-the-unprivileged-cli.md)
- [0005 A fixed tap MAC](adr/0005-fixed-tap-mac.md)
- [0006 State commits don't fsync](adr/0006-state-commits-dont-fsync.md)
- [0007 The edge is hyper in the agent](adr/0007-edge-in-the-agent-on-hyper.md)
- [0008 ACME over HTTP-01 from the edge](adr/0008-acme-over-http-01-from-the-edge.md)

# Measurements

- [guest-init boot speed (M2)](m2s2-speed-test.md)
