# cirrocumulus stays a single-machine system; pooling lives in The Loam

cirrocumulus is one privileged agent per host, a local Unix socket, one SQLite database, and an edge for that host's Apps. Its roadmap (M8, `BUILD_PATHWAY.md` and `RESEARCH.md` §2 "Tiling") planned to grow that into a cluster: a `cirro server` that owns desired state, `cirro node join` with one-time tokens, a cluster CA with mTLS between Nodes, a scheduler, and a WireGuard mesh. That plan is not going ahead inside cirrocumulus. A personal cluster of one person's machines does not make sense yet, and the case that does, several people lending machines to a shared pool, is a different product: it needs member identity, owner policy that overrides the controller, usage records and a trust model between people, none of which a single-owner tool should carry.

That work is a separate project, The Loam (`~/cs/the-loam`), which drives cirrocumulus as its preferred execution surface through the agent's Unix socket. Only The Loam's own worker faces the network; the cirrocumulus agent never does. Cluster CA, join, scheduling, a ledger and a cross-node ingress are The Loam's.

## Considered options

- **Build M8 inside cirrocumulus and let The Loam sit on top.** One codebase, but it makes a single-machine tool carry cluster identity and scheduling before anything needs them, and the pool's needs (owners, policy that wins, member credentials) would pull the cluster design in a direction that single-owner users do not want. Rejected.
- **Keep the M8 plan and also build The Loam.** Two control planes with overlapping jobs. Rejected.

## Consequences

- `cirro server init` and `cirro node join`, which exit "not yet implemented", are not planned; M8's server, join, cluster CA and WireGuard-mesh items are dropped from the roadmap. The Machines work (ADR 0010) is kept: it is a single-machine feature.
- A request from The Loam is accepted only if a single machine would want it on its own merit: for example, running a VM to completion with an exit code, or copying a file out of an Ended VM. Today a finished VM gives an end reason and a console log, and neither of those exists. Nothing cluster-shaped is added.
- Still to do, not done by this ADR: update the M8 to M11 rows of `BUILD_PATHWAY.md`, `RESEARCH.md` §2 and §6, the roadmap lines in `README.md`, and the `server init` and `node join` entries in the CLI. M9 to M11 items that only make sense across hosts (peer chunk fetch, node drain) need a second look.
