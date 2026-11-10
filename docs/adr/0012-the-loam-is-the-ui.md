# Cirrocumulus has no UI of its own; The Loam is the UI, the `cirro` CLI stays

Cirrocumulus is operated from the `cirro` CLI, and that is all it ships for people. The live `cirro top` dashboard (the `cirro-tui` crate) is removed, and the planned Dioxus desktop console (`cirro-gui`, issue #57) is not built. Seeing and acting on machines through a screen is The Loam's job: its pool dashboard, owner panel, `loam top` and edge veil show Nodes, their VMs, tasks and Apps, and reach each Node only through The Loam worker on that host, which talks to the Node agent's socket (ADR 0011). `cirro top` stays as a command but prints one plain-text snapshot of the Node and its VMs and exits, the output `cirro top --once` gave before; `cirro ps`, `cirro logs` and the rest are unchanged.

## Considered options

- **Keep the live `cirro top` and build `cirro-gui` as planned.** A single-machine user gets screens without running The Loam, but there would be three UIs for the same VMs (TUI, desktop GUI, The Loam), each needing its own upkeep as the agent's API grows. Rejected.
- **Keep the live `cirro top` and drop only the GUI.** Smaller change, but the dashboard is still a second UI to keep in step with The Loam's, and its stop and log keys are actions the CLI already has. Rejected.

## Consequences

- A single-machine user without The Loam has the CLI only: `cirro top` for a snapshot, `watch cirro top` for a refreshing one, `cirro logs` and `cirro stop` for what the dashboard's keys did.
- The Loam's worker acts only on the `loam-` VMs it created; VMs started with `cirro run` are shown read-only, only in the owner panel on that machine, and are managed with the CLI.
- The spec for the desktop console (#57), its tickets (#58 to #63) and the client move it needed (#64, PR #65, whose ADR also took the number 0012) are to be closed as not planned.
- The terminal UI library leaves the dependency tree; the plain-text formatters move into the CLI.
