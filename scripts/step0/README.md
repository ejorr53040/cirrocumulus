Step 0 verification: can this machine boot a Firecracker microVM under
`jailer` at all, before any Rust gets written (see `BUILD_PATHWAY.md`).

This is manual, exploratory scaffolding from before `cirro-node` existed.
For an actual Node, `cirro node install` (#11) is the real, automated
prereq check and setup (KVM, cgroup v2, fetching a pinned, SHA-256-verified
Firecracker/jailer/kernel, the `cirro` group, a systemd unit) -- these
scripts stay only for poking at Firecracker+jailer directly while working
on this crate.

## One-time setup

```sh
sudo pacman -S busybox --noconfirm
echo 'ejorr ALL=(root) NOPASSWD: /home/ejorr/cs/cirrocumulus/jailer' | sudo tee /etc/sudoers.d/cirrocumulus-jailer
sudo chmod 440 /etc/sudoers.d/cirrocumulus-jailer
```

`jailer` needs root (chroot, cgroups, device chown, privilege drop). The
sudoers rule above grants passwordless root for that exact binary only, so
the tests can invoke it non-interactively. Remove
`/etc/sudoers.d/cirrocumulus-jailer` when you're done with Step 0 if you'd
rather not leave it in place.

**This is equivalent to unrestricted passwordless root for `ejorr`, not a
narrow capability grant** (2026-09-28 security audit, #5): `jailer` itself
takes caller-chosen `--exec-file`/`--uid`/`--gid`, so `sudo jailer
--exec-file /bin/sh --uid 0 --gid 0 ...` is a root shell. Scoping the
sudoers rule to this one binary path (rather than a wildcard) is correct
and intentional, but don't mistake it for sandboxing the *arguments* --
it isn't. Fine for a single-user dev machine; if this pattern is ever
reused as a template for a real multi-user host, restrict `--uid`/`--gid`
to a fixed non-root range and `--exec-file` to the pinned firecracker
binary via a sudoers `Cmnd_Alias` with fixed arguments, not just a fixed
program path. Production doesn't use `sudo` at all for exactly this reason
-- the real Node agent runs `jailer` directly as an already-root systemd
service (`cirro node install`, #11).

## Running the tests

```sh
./fetch_kernel.sh    # downloads a CI-built vmlinux (once; skips if present)
./build_rootfs.sh     # builds a minimal busybox rootfs (rerun after editing it)
./build_rootfs_guest_init.sh  # cross-compiles guest-init (M2) and rootfs's it
./build_rootfs_nat.sh  # busybox rootfs whose guest configures eth0 and probes 1.1.1.1:443 (M3 NAT)
./tests/run.sh
```

`build_rootfs_nat.sh` isn't used by `tests/run.sh`. It was built for
`cirro-node`'s `jailed_boot_with_nat_reaches_the_internet` test
(`tests/jailer_network.rs`), retired in #11 along with the `sudo`-scoped
`create_persistent_tap_owned_by`/`configure_link_via_sudo`/`enable_nat`/
`disable_nat` helpers it exercised -- the real Node agent sets up
networking itself (per-VM network namespaces, ADR 0001) rather than
shelling out to a narrowly `sudo`-scoped `ip`/`nft`, so those helpers and
their sudoers rules are gone (2026-09-28 security audit, #6). **If
`/etc/sudoers.d/cirro-tap` or `/etc/sudoers.d/cirro-nft` exist on this
host from before #11, remove them** -- nothing in this repo grants or
needs those rules anymore, and a stale sudoers file granting passwordless
`ip`/`nft` access outlives whichever branch originally set it up.
NAT/egress-reaching-the-internet is now
covered by `crates/cirro/tests/node_agent.rs`'s
`vms_reach_the_internet_but_not_smtp_each_other_or_the_lan`, against the
real agent.

**`crates/cirro-node/src/jailer.rs`'s `Jail::spawn` (the `sudo jailer` path
this file's own "One-time setup" above sets up) is the other dead sudo
surface #6 names, and it's deliberately still here** -- unlike
`network.rs`'s helpers, it's still exercised by `crates/cirro-node/tests/jailer.rs`
against the checked-in `firecracker`/`jailer` binaries this dev harness
uses, and retiring it means retiring those binaries and every fixture that
references them too. Judged a separate, larger, riskier change than this
fix, so it's left open rather than silently folded into "done": production
still doesn't use `sudo` for this (`cirro node install`'s systemd unit runs
`jailer` directly, already root), so nothing here is reachable outside this
dev harness -- but the residual sudo surface itself, and the checked-in
binaries it depends on, are still real and still unretired.

Four seams are tested, each through the real Firecracker API socket (no
mocks):

- `prereqs.sh` — KVM device, virtualization extension, cgroup version,
  binary presence/versions, passwordless sudo for jailer, guest build tools.
- `run_plain.sh` — plain `firecracker` (no jailer) boots the kernel+rootfs
  to userspace.
- `run_jailer.sh` — the same boot, wrapped in `jailer`: chrooted, running as
  an unprivileged uid, still reaching userspace.
- `run_guest_init.sh` — the same kernel, but `/init` is the real
  `crates/guest-init` binary (M2), cross-compiled static for
  `x86_64-unknown-linux-musl`, not busybox. Proves guest-init mounts
  `/proc`/`/sys`/`/dev` and holds PID 1 open as a real microVM's init would;
  exec/reap/shutdown/vsock land in later M2 slices with their own tests.

Boot scripts print the guest's serial console and exit 0 iff they see the
expected marker before a 10s timeout (`STEP0_BOOT_OK` for the busybox
scripts, `GUEST_INIT_MOUNTS_OK` for `run_guest_init.sh`).

## Cleanup

```sh
./clean.sh
```

Reclaims the disk space `run_jailer.sh` leaves behind under
`~/.cirrocumulus-step0-jail` (see Notes below for why it accumulates).
Needs sudo, since the directories jailer leaves root-owned can't be removed
without it.

## Notes

- `jailer` needs its chroot base directory on a filesystem *without* the
  `nodev` mount option — it mknods `/dev/kvm` inside the chroot, and device
  nodes are inert on `nodev` mounts (surfaces as a confusing "Permission
  denied" from Firecracker's own KVM init, not an obvious mount error).
  `run_jailer.sh` uses `$HOME` for this reason (`/tmp` is `nodev` here).
- `jailer` chowns only the leaf `<chroot-base>/firecracker/<id>/root/`
  directory to the unprivileged uid; the `firecracker/` and `<id>/`
  directories above it stay root-owned, so we can't clean them up between
  runs without sudo. `run_jailer.sh` uses a fresh id (`s0-$$`) per run
  instead of trying to delete the old one. Run `./clean.sh` occasionally to
  reclaim space.
