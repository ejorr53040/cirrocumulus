# The Node agent's state commits don't fsync

The agent keeps its VM records in SQLite (`state.db`) in WAL mode with `synchronous = NORMAL`, so a commit doesn't wait for the disk. A wake records the VM after it is running, and with a commit that fsyncs, the record was a fifth of a wake: about 34 ms under SQLite's default rollback journal, and on btrfs a WAL fsync also waits for the park's snapshot, just written, to reach disk, which pushed wake p99 from 54 ms to 154 ms. Without the fsync, wake meets its 100 ms p99 goal (43 ms p50, 54 ms p99).

## Considered options

- **WAL with `synchronous = FULL`.** One fsync per commit, durable across power cuts. Rejected for the btrfs tail above.
- **Record the woken VM after answering the CLI.** The wake would return sooner, but an agent that died in between would leave a running VM with no record, which the next start treats as an orphan and kills.

## Consequences

WAL with NORMAL still survives the agent crashing or being killed: only a power cut or kernel crash can lose commits, and then only the last ones. Any record can be lost that way, not just a wake's: a `run` (the VM, gone anyway after a power cut, is forgotten and its leftovers swept), an `rm` (the ended VM reappears in `ps -a`), or a park (the VM shows as having died, and its snapshot is swept as one no record owns). A parked VM never survived a power cut reliably anyway, because nothing fsyncs a snapshot either. If parked VMs are to survive power cuts, park must fsync its snapshot and the park's commit, and that cost belongs to park, not wake.
