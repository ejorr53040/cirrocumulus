# Cirrocumulus

[![CI](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml/badge.svg)](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml)

**A secure, tileable mini cloud you run yourself.**
Every workload gets its own Firecracker microVM, locked down with jailer.
Manage everything from your terminal.

> Pre-alpha: the VM layer (guest init, boot, jailer, tap networking) works; the
> `cirro` subcommands are not wired up yet.

## Development

Needs Linux, `rustup` (the toolchain and musl target are pinned in
`rust-toolchain.toml`), and `/dev/kvm` for the VM tests.

```sh
scripts/ci/install-hooks.sh   # once: commit-msg + pre-push hooks
scripts/ci/local.sh           # the same gates CI runs (--quick skips docs + tests)
```

All gates in `.github/workflows/ci.yml` block merging, including commit-message
rules and a check that every commit builds. The real-Firecracker tests only
compile in CI, so run them locally: see `scripts/step0/README.md`.
