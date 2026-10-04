# Cirrocumulus compared with Coolify, Dokku, Proxmox and Fly.io Machines

| | Isolation | Interface | More hosts | Scale to zero | License |
| --- | --- | --- | --- | --- | --- |
| **Cirrocumulus** | Firecracker microVM + jailer | CLI + TUI | Planned (v0.2) | Yes: park and wake | Apache-2.0 |
| Coolify | Docker containers | Web UI | Servers over SSH | No | Apache-2.0 |
| Dokku | Docker containers | CLI, `git push` | Mostly one host | No | MIT |
| CapRover | Docker Swarm | Web UI + CLI | Swarm | No | Apache-2.0 |
| Proxmox VE | KVM/QEMU VMs and LXC | Web UI | Cluster | No | AGPL-3.0 |
| Incus | System containers and QEMU VMs | CLI + web UI | Cluster | No | Apache-2.0 |
| Fly.io Machines | Firecracker | CLI + API, hosted | Global | Stop and suspend | Proprietary |

**Against Coolify, Dokku and CapRover:** they run your apps as containers,
which share the host's kernel; a kernel bug reachable from one app is reachable
from all of them. Cirrocumulus gives each its own kernel. They have more of a
deploy story (git push, buildpacks, web UIs) today.

**Against Proxmox and Incus:** they manage general-purpose VMs and containers
and leave the app layer to you. Cirrocumulus's VMs boot from OCI images in
about a second, park when idle, and are reached by hostname through its edge.

**Against Fly.io Machines:** the same isolation (Firecracker), on hardware you
own, with no per-machine bill; Fly runs it for you across the world.

**Pick something else if** you need a web UI, Windows guests, GPUs, live
migration, or more than one host today.
