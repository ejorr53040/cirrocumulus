Step 0 verification: can this machine boot a Firecracker microVM at all,
before any Rust gets written (see `BUILD_PATHWAY.md`).

This is manual, exploratory scaffolding from before `cirro-node` existed.
A real Node is set up by `cirro node install`, which checks KVM and cgroup v2,
fetches a pinned, SHA-256-verified Firecracker, jailer and kernel, creates the
`cirro` group and installs the systemd unit. These scripts remain for poking at
Firecracker directly, and `fetch_kernel.sh` fetches the kernel the VM tests boot.

## One-time setup

```sh
sudo pacman -S busybox --noconfirm
```

Nothing here needs `sudo` beyond that. The jailed boot script and its
passwordless-root `sudoers` rule for `jailer` are gone: `jailer` takes
caller-chosen `--exec-file`/`--uid`/`--gid`, so that rule was a root shell, and
the Node agent's own tests cover jailed boots. If
`/etc/sudoers.d/cirrocumulus-jailer` exists on this host, remove it.

## Running the tests

```sh
./fetch_kernel.sh    # downloads a CI-built vmlinux (once; skips if present)
./build_rootfs.sh     # builds a minimal busybox rootfs (rerun after editing it)
./build_rootfs_guest_init.sh  # cross-compiles guest-init (M2) and rootfs's it
./build_rootfs_nat.sh  # busybox rootfs whose guest configures eth0 and probes 1.1.1.1:443 (M3 NAT)
./tests/run.sh
```

`build_rootfs_nat.sh` isn't used by `tests/run.sh`. It served a NAT test whose
`sudo ip`/`sudo nft` helpers #11 retired; the Node agent now sets up networking
itself (ADR 0001). If `/etc/sudoers.d/cirro-tap` or `/etc/sudoers.d/cirro-nft`
exist on this host from before #11, remove them: nothing needs them, and a
stale passwordless `ip`/`nft` rule outlives the branch that added it.
Internet egress is covered by `crates/cirro/tests/node_agent.rs`
(`vms_reach_the_internet_but_not_smtp_each_other_or_the_lan`).

Three seams are tested, each through the real Firecracker API socket (no
mocks):

- `prereqs.sh` — KVM device, virtualization extension, cgroup version,
  binary presence/versions, guest build tools.
- `run_plain.sh` — plain `firecracker` (no jailer) boots the kernel+rootfs
  to userspace.
- `run_guest_init.sh` — the same kernel, but `/init` is the real
  `crates/guest-init` binary (M2), cross-compiled static for
  `x86_64-unknown-linux-musl`, not busybox. Proves guest-init mounts
  `/proc`/`/sys`/`/dev` and holds PID 1 open as a real microVM's init would;
  exec/reap/shutdown/vsock land in later M2 slices with their own tests.

Boot scripts print the guest's serial console and exit 0 iff they see the
expected marker before a 10s timeout (`STEP0_BOOT_OK` for the busybox
scripts, `GUEST_INIT_MOUNTS_OK` for `run_guest_init.sh`).
