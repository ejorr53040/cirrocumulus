# Firecracker snapshots in practice: park and wake

`cirro park <name>` snapshots a VM to disk and ends it, freeing its RAM.
`cirro wake <name>` starts a new VM from that snapshot, and the guest carries
on where it was. With an App's `--idle-park`, the Node does both on its own:
it parks an App that has had no requests for a while, and wakes it when the
next request arrives.

## What happens

**Park:** pause the VM (`PATCH /vm {"state": "Paused"}`), write a full
snapshot (`PUT /snapshot/create`: the device state and all of guest memory),
kill Firecracker, and move the snapshot and the VM's rootfs out of its jail
into a root-only directory. If the snapshot fails, the VM is resumed and keeps
running.

**Wake:** a new jail, network namespace and Firecracker, the snapshot moved
in, then `PUT /snapshot/load` with `resume_vm: true`. The snapshot is used up:
each snapshot wakes once.

## What it costs

`cirro bench --runs 50`, 256 MiB guest, i9-13900H laptop, NVMe, btrfs:

| | p50 | p99 |
| --- | ---: | ---: |
| park | 274 ms | 1854 ms |
| wake | 43 ms | 54 ms |

Loading and resuming the snapshot is about 10 ms of a wake. Most of the rest
is starting Firecracker under jailer, with the network namespace set up while
it starts. Park's tail is writing 256 MiB to disk.

## What we learned

- **The guest keeps its network state.** Its address, its ARP cache and its
  open sockets are as they were. Every VM's guest has the same address inside
  its own namespace ([ADR 0003](adr/0003-fixed-guest-address.md)), and every
  tap the same MAC ([ADR 0005](adr/0005-fixed-tap-mac.md)), so a woken guest's
  stale ARP entry for its gateway is still right. Without the fixed MAC, a
  guest that spoke first after a wake couldn't reach the internet for over
  10 s.
- **Randomness.** Two wakes of one snapshot would resume the same RNG state.
  Firecracker's VMGenID device tells the guest kernel it has been restored,
  and the kernel reseeds (you'll see `crng reseeded due to virtual machine
  fork` in the console log). Cirrocumulus also uses each snapshot only once.
- **Snapshots are tied to the host.** Firecracker's snapshots work only on
  the same Firecracker version and a compatible host kernel and CPU, so a
  parked VM can't be moved to an arbitrary host or survive every upgrade.
- **Durability.** Nothing fsyncs a snapshot, so a power cut can lose a parked
  VM ([ADR 0006](adr/0006-state-commits-dont-fsync.md)).
