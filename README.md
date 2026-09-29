# Cirrocumulus

[![CI](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml/badge.svg)](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.1.0-informational.svg)](Cargo.toml)

**A secure, tileable mini cloud you run yourself.**
Every workload gets its own Firecracker microVM, locked down with jailer.
Manage everything from your terminal.

> Pre-alpha: single-node VM lifecycle (`node install`, `run`, `ps`, `logs`,
> `ssh`, `stop`, `rm`, `park`, `wake`) is wired up; the multi-node control
> plane (`cirro server`, `cirro node join`) is not.

## Install

Needs a Linux x86_64 host with KVM (`/dev/kvm`) and cgroup v2, and systemd
(the Node agent runs as a systemd unit).

```sh
cargo build --release -p cirro
sudo install -m 755 target/release/cirro /usr/local/bin/cirro
```

## Run

Set up the host once, as root -- checks prereqs, fetches and verifies the
pinned Firecracker/jailer/kernel release, creates the `cirro` group and
state dir, and installs + starts the Node agent as a systemd unit:

```sh
sudo cirro node install
```

Then boot a VM from an ext4 rootfs with `guest-init` as its `/init`
(`scripts/apps/uvm-career-quiz/build_rootfs.sh` shows how to build one):

```sh
cirro run --name web --mem 256M --vcpus 1 rootfs.ext4 -- /entrypoint
```

`cirro run` talks to the Node agent over its socket (`/run/cirro/agent.sock`
by default, `--socket`/`$CIRRO_SOCKET` to override) and prints the VM's
address on success.

## Usage

```sh
cirro run --name <name> [--mem 256M] [--vcpus 1] <rootfs> -- <command> [args...]
cirro ps [--all]                  # list VMs (--all includes Ended VMs)
cirro logs [--follow] <name>      # a VM's console log
cirro ssh <name>                  # open a shell in a VM
cirro stop [--force] [--timeout <secs>] <name>
cirro rm <name>                   # delete an Ended VM's record and log
cirro park <name>                 # snapshot a VM to disk, free its RAM
cirro wake <name>                 # restore a parked VM
cirro top                         # live terminal dashboard
cirro bench                       # measure boot, park and wake times
cirro node uninstall [--force]    # reverse `node install`
```

Run `cirro --help` or `cirro <command> --help` for the full flag list.

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
