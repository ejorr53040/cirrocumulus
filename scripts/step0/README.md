Step 0 verification: can this machine boot a Firecracker microVM under
`jailer` at all, before any Rust gets written (see `BUILD_PATHWAY.md`).

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

## Running the tests

```sh
./fetch_kernel.sh    # downloads a CI-built vmlinux (once; skips if present)
./build_rootfs.sh     # builds a minimal busybox rootfs (rerun after editing it)
./tests/run.sh
```

Three seams are tested, each through the real Firecracker API socket (no
mocks):

- `prereqs.sh` — KVM device, virtualization extension, cgroup version,
  binary presence/versions, passwordless sudo for jailer, guest build tools.
- `run_plain.sh` — plain `firecracker` (no jailer) boots the kernel+rootfs
  to userspace.
- `run_jailer.sh` — the same boot, wrapped in `jailer`: chrooted, running as
  an unprivileged uid, still reaching userspace.

Both boot scripts print the guest's serial console and exit 0 iff they see
the guest's own `STEP0_BOOT_OK` marker before a 10s timeout.

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
