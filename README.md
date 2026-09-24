# Cirrocumulus

**A secure, tileable mini cloud you run yourself.**
Every workload gets its own Firecracker microVM, locked down with jailer.
Manage everything from your terminal.

[badges: CI | license | crates.io | latest release]

[GIF: `cirro up`, launch a VM, then the live dashboard showing CPU/mem/net per VM]

## Why Cirrocumulus?

Big clouds are overkill for a club server, a small company, or a few
hundred users. Container-based self-hosting tools share a kernel between
tenants. Cirrocumulus gives each workload hardware-virtualized isolation
(the same technology behind AWS Lambda and Fargate) in a single Rust binary
you can run on a spare machine, then tile out across more nodes as you grow.

## Features
- Hard isolation: one Firecracker microVM per workload, jailed with seccomp, cgroups, namespaces
- Boots in ~125 ms, tiny memory footprint
- Add identical nodes to scale out
- Live terminal analytics: per-VM and per-node CPU, memory, disk, network
- Single static binary, no daemon sprawl

## Quick start
...
The dashboard GIF is your best asset. Terminal monitoring tools spread by screenshot (btop's README is mostly images), so:
Record with vhs (Charm) or asciinema plus agg, which makes clean, reproducible GIFs.
Put a static screenshot of the dashboard at the very top and the GIF right below it.
Use a good terminal theme and a tall enough window that the dashboard fills the frame.
