# Running Firecracker with jailer

[jailer](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md)
is the Firecracker project's own sandbox for the VMM process: it puts
Firecracker in a chroot, new mount and PID namespaces, a network namespace you
name, and cgroups, then drops to an unprivileged uid and execs Firecracker.
Cirrocumulus runs every VM this way. These are the things that cost us time.

## What the Node agent passes

```sh
jailer --id cirro-e202 \
  --exec-file /var/lib/cirro/release/firecracker \
  --uid 957858 --gid 957858 \
  --chroot-base-dir /var/lib/cirro/jail \
  --netns /run/netns/cirro-e202 \
  --cgroup-version 2 --parent-cgroup cirro \
  --cgroup memory.max=301989888 --cgroup cpu.max="100000 100000"
```

- **One uid per VM.** Each VM's Firecracker runs as its own unprivileged uid,
  so an escaped VMM can't signal or ptrace another VM's.
- **One network namespace per VM** (`--netns`), made before jailer starts,
  holding the VM's tap. The namespace joins the Node through a veth pair, and
  the Node routes to it ([ADR 0001](adr/0001-per-vm-network-namespace.md)).
- **cgroup v2 limits** (`--cgroup-version 2`): the guest's memory plus the
  VMM's own overhead, and one CPU per vCPU. jailer turns on only the
  controllers it limits; Cirrocumulus turns on `io` in the parent cgroup
  itself, to count disk use for `cirro top`.
- **jailer execs into Firecracker without forking**, so the pid you spawn is
  Firecracker's. Record it with its start time (from `/proc/<pid>/stat`), so a
  restarted agent can tell its VMs from processes that reused their pids.

## Gotchas

- **jailer needs root.** It chroots, mknods, chowns and joins cgroups. Run
  the thing that spawns it as a root service (the Node agent is a systemd
  unit, [ADR 0002](adr/0002-privileged-node-agent.md)), not through `sudo`.
  A sudoers rule for jailer is passwordless root: `--exec-file` and `--uid`
  are the caller's choice.
- **The chroot base must not be `nodev`.** jailer mknods `/dev/kvm` and
  `/dev/net/tun` inside the chroot, and device nodes are inert on a `nodev`
  mount. The symptom is a "Permission denied" from Firecracker's KVM setup,
  not a mount error. `/tmp` is often `nodev`; `/var/lib` usually isn't.
- **Socket paths are short.** Firecracker's API socket lives under
  `<chroot-base>/firecracker/<id>/root/`, and a Unix socket path must fit in
  108 bytes. Keep the base directory and ids short.
- **Only the leaf is the VM's.** jailer chowns `<chroot-base>/firecracker/<id>/root`
  to the VM's uid; the directories above it stay root's. Clean up the whole
  `<id>` directory as root when the VM ends.
- **Files go into the jail, not paths.** Firecracker sees only the chroot, so
  the kernel, rootfs and snapshot files are hard-linked or copied in, owned by
  the VM's uid, and named by their path inside it.
- **Firecracker hides VMX from guests**, so you can't run Firecracker (or
  Cirrocumulus) inside a Firecracker VM.
