# Graph Report - cirrocumulus  (2026-09-24)

## Corpus Check
- Corpus is ~5,891 words - fits in a single context window. You may not need a graph.

## Summary
- 119 nodes · 127 edges · 24 communities (6 shown, 18 thin omitted)
- Extraction: 97% EXTRACTED · 2% INFERRED · 1% AMBIGUOUS · INFERRED: 3 edges (avg confidence: 0.8)
- Token cost: 61,796 input · 0 output

## Community Hubs (Navigation)
- Step 0 Boot Harness
- CLI Subcommand Tree
- Guest-Init PID 1 Core
- Guest-Init Config Parsing
- M2 Speed Test & Rootfs Build
- Guest-Init Boot Test Harness
- Test Assertion Library
- Jailer Boot Test
- Plain Boot Test
- Prereqs Test
- cirro-edge package
- cirro-image package
- cirro-node package
- cirro-proto package
- cirro-server package
- cirro-tui package
- cirrocumulus (cirro bin) package
- guest-init package

## God Nodes (most connected - your core abstractions)
1. `Step 0 Verification (scripts/step0/README.md)` - 12 edges
2. `Command` - 8 edges
3. `M2 Slice 2 Time-to-Start Speed Test` - 8 edges
4. `Config` - 6 edges
5. `parse_config()` - 6 edges
6. `cirro()` - 4 edges
7. `jailer` - 4 edges
8. `NodeCommand` - 3 edges
9. `DbCommand` - 3 edges
10. `exec_configured_app()` - 3 edges

## Surprising Connections (you probably didn't know these)
- `Cirrocumulus` --references--> `jailer`  [INFERRED]
  README.md → scripts/step0/README.md
- `M2 Slice 2 Time-to-Start Speed Test` --references--> `Firecracker microVM`  [EXTRACTED]
  m2s2_speed_test.md → README.md
- `Step 0 Verification (scripts/step0/README.md)` --references--> `Firecracker microVM`  [EXTRACTED]
  scripts/step0/README.md → README.md
- `exec_configured_app()` --references--> `Config`  [EXTRACTED]
  crates/guest-init/src/main.rs → crates/guest-init/src/config.rs
- `cirro()` --references--> `Command`  [EXTRACTED]
  crates/cirro/tests/cli.rs → crates/cirro/src/main.rs

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Guest-init boot and speed-measurement pipeline** — m2s2_speed_test_guest_init, scripts_step0_run_guest_init, scripts_step0_build_rootfs_guest_init, scripts_step0_speed_test_guest_init, scripts_step0_fixtures_child_app [INFERRED 0.85]
- **jailer chroot setup, caveats, and cleanup lifecycle** — scripts_step0_readme_jailer, scripts_step0_readme_nodev_chroot_caveat, scripts_step0_readme_jailer_chown_leaf_only, scripts_step0_run_jailer, scripts_step0_clean [EXTRACTED 1.00]

## Communities (24 total, 18 thin omitted)

### Community 0 - "Step 0 Boot Harness"
Cohesion: 0.11
Nodes (16): Cirrocumulus, Firecracker microVM, BUILD_PATHWAY.md, build_rootfs.sh script, clean.sh script, fetch_kernel.sh script, check(), prereqs.sh script (+8 more)

### Community 1 - "CLI Subcommand Tree"
Cohesion: 0.23
Nodes (13): clap, Cli, Command, DbCommand, main(), NodeCommand, String, ServerCommand (+5 more)

### Community 2 - "Guest-Init PID 1 Core"
Cohesion: 0.18
Nodes (12): BOOT_MARKER, CONFIG_PATH, exec_configured_app(), main(), mount_pseudo_filesystems(), cstring, duration, mount (+4 more)

### Community 3 - "Guest-Init Config Parsing"
Cohesion: 0.21
Nodes (10): Config, defaults_args_to_empty_when_omitted(), parse_config(), parses_exec_path_and_args(), String, deserialize, Error, Result (+2 more)

### Community 4 - "M2 Speed Test & Rootfs Build"
Cohesion: 0.20
Nodes (7): /etc/cirro-init.json config, M6 — validates the core bet (snapshot-restore park/wake), M2 Slice 2 Time-to-Start Speed Test, build_rootfs_guest_init.sh script, api(), speed_test_guest_init.sh script, TIMES_MS

### Community 5 - "Guest-Init Boot Test Harness"
Cohesion: 0.20
Nodes (5): guest-init (M2), Slice 1 panic-on-return bug, api(), run_guest_init.sh script, test_guest_init_boot.sh script

## Ambiguous Edges - Review These
- `run_guest_init.sh` → `test_guest_init_boot.sh`  [AMBIGUOUS]
  m2s2_speed_test.md · relation: semantically_similar_to

## Knowledge Gaps
- **24 isolated node(s):** `cirro-edge`, `cirro-image`, `cirro-node`, `cirro-proto`, `cirro-server` (+19 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 63 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **18 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `run_guest_init.sh` and `test_guest_init_boot.sh`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **Why does `Step 0 Verification (scripts/step0/README.md)` connect `Step 0 Boot Harness` to `M2 Speed Test & Rootfs Build`, `Guest-Init Boot Test Harness`, `Test Assertion Library`?**
  _High betweenness centrality (0.138) - this node is a cross-community bridge._
- **Why does `M2 Slice 2 Time-to-Start Speed Test` connect `M2 Speed Test & Rootfs Build` to `Step 0 Boot Harness`, `Guest-Init Boot Test Harness`?**
  _High betweenness centrality (0.058) - this node is a cross-community bridge._
- **Why does `Config` connect `Guest-Init Config Parsing` to `Guest-Init PID 1 Core`?**
  _High betweenness centrality (0.023) - this node is a cross-community bridge._
- **What connects `cirro-edge`, `cirro-image`, `cirro-node` to the rest of the system?**
  _24 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Step 0 Boot Harness` be split into smaller, more focused modules?**
  _Cohesion score 0.10869565217391304 - nodes in this community are weakly interconnected._