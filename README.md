<p align="center">
  <img src="docs/assets/banner.png" alt="Cirrocumulus" width="100%">
</p>

[![CI](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml/badge.svg)](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.1.0-informational.svg)](Cargo.toml)

**A secure, tileable mini cloud you run yourself.**
Every workload gets its own Firecracker microVM, locked down with jailer.
Manage everything from your terminal.

> Pre-alpha: the single-node VM lifecycle (`node install`, `run` from an OCI
> image or a rootfs, `image`, `ps`, `top`, `logs`, `stop`, `rm`,
> `node uninstall`) works. `ssh`, `park`, `wake`, `bench`, `db` and the
> multi-node control plane (`server`, `node join`) are listed in `--help` but
> exit with "not yet implemented".

## Install

Needs a Linux x86_64 host with KVM (`/dev/kvm`) and cgroup v2, and systemd
(the Node agent runs as a systemd unit).

```sh
cargo build --release -p cirrocumulus
sudo install -m 755 target/release/cirro /usr/local/bin/cirro
```

## Run

Set up the host once, as root. This checks prereqs, fetches and verifies the
pinned Firecracker, jailer and kernel release, creates the `cirro` group and
state dir, and installs and starts the Node agent as a systemd unit:

```sh
sudo cirro node install
```

Then boot a VM from an OCI image, and reach it at the address it prints:

```sh
cirro run --name web nginx:alpine
curl http://10.77.0.2/
```

The CLI pulls the image and builds its rootfs as you, not as root, and caches it
in `~/.cache/cirro/images`. [A Firecracker rootfs from a Docker
image](docs/rootfs-from-an-image.md) explains how, and its limits: public
`linux/amd64` images only, for now. `cirro run` also boots an ext4 rootfs you
built yourself, given a path and a command:
`cirro run --name web ./rootfs.ext4 -- /entrypoint`.

`cirro run` talks to the Node agent over its socket (`/run/cirro/agent.sock`
by default, `--socket`/`$CIRRO_SOCKET` to override) and prints the VM's
address on success.

## Usage

```sh
cirro run --name <name> [--mem 256M] [--vcpus 1] [-e KEY=VALUE]... [-w DIR] [-u UID:GID] \
          <image> [-- <command> [args...]]
cirro run --name <name> [flags...] <rootfs> -- <command> [args...]
cirro image pull <image>          # pull and build ahead of time
cirro image ls                    # cached images
cirro image rm <image|digest>
cirro ps [--all]                  # list VMs (--all includes Ended VMs)
cirro top [--once]                # live CPU, memory, disk and network use
cirro logs [--follow] <name>      # a VM's console log
cirro stop [--force] [--timeout <secs>] <name>
cirro rm <name>                   # delete an Ended VM's record and log, or a parked VM's snapshot
cirro park <name>                 # snapshot a VM to disk and free its RAM
cirro wake <name>                 # start it again from its snapshot; prints its VM address
cirro bench [--runs 10] <image|rootfs> [-- <command>]   # time boot, park and wake
cirro node uninstall [--force]    # reverse `node install`
```

Run `cirro --help` or `cirro <command> --help` for the full flag list.

## Park and wake

`cirro park` pauses a VM, writes a full Firecracker snapshot of its memory and
devices, and ends it, freeing its RAM; the snapshot and the VM's rootfs stay in
the state dir. `cirro wake` starts a new VM under the same name from that
snapshot, so the guest carries on where it was rather than booting again. It may
get a different VM address: every guest has the same Guest address inside its
own namespace ([ADR 0003](docs/adr/0003-fixed-guest-address.md)), and the same
tap MAC ([ADR 0005](docs/adr/0005-fixed-tap-mac.md)).

`cirro bench --runs 50` on an i9-13900H laptop (20 threads, 16 GiB, NVMe,
btrfs, Linux 7.2), with a 256 MiB guest running the test suite's `counter`
HTTP server, timing each request as the CLI sees it. A boot includes the 2 s
the agent waits to see a new VM stay up, so the boot itself is about 1.1 s;
park and wake have no such wait.

| Operation | p50 | p99 |
| --- | ---: | ---: |
| boot | 3155 ms | 3773 ms |
| park | 274 ms | 463 ms |
| wake | 158 ms | 409 ms |

Wake misses the goal of a 100 ms p99. Firecracker's own log put one wake's
snapshot load at about 17 ms, so most of the time is likely the host-side setup
around it (network namespace, veth, NAT and jailer, each a separate
process, then moving the snapshot into the jail); profiling it is the next step.

## Development

Needs Linux, `rustup` (the toolchain and musl target are pinned in
`rust-toolchain.toml`), and `/dev/kvm` for the VM tests.

```sh
scripts/ci/install-hooks.sh   # once: commit-msg + pre-push hooks
scripts/ci/local.sh           # the same gates CI runs (--quick skips docs + tests)
```

All gates in `.github/workflows/ci.yml` block merging, including commit-message
rules and a check that every commit builds. The real-Firecracker tests only
compile in CI, so run them locally. `crates/cirro/tests/node_agent.rs` explains the
one `sudoers` rule they need, and `scripts/step0/README.md` covers fetching the
kernel.
