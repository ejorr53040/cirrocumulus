# M2 slice 2 — time-to-start speed test

**What this measures:** wall-clock time from issuing Firecracker's
`InstanceStart` API call to `CHILD_APP_RAN` appearing on the guest's serial
console — i.e. the point at which guest-init has mounted `/proc`/`/sys`/`/dev`,
read `/etc/cirro-init.json`, forked, and successfully exec'd the configured
app. This is the number a request-triggered wake would actually block on.

**What this is not:** a measurement of the park/wake (snapshot-restore) path.
That's M6 — "validates the core bet" — and is expected to land far below this
number. This is the cold-boot floor as of M2 slice 2, before shutdown, vsock,
or any restore path exist.

## Method

- Script: `scripts/step0/speed_test_guest_init.sh` (new, not part of the
  correctness test suite — those poll at 100ms resolution, too coarse to say
  anything meaningful about a boot this fast).
- Each of 20 runs boots a fresh Firecracker process against a fresh API
  socket (no VM reuse). Sequence per run: `PUT /machine-config`,
  `/boot-source`, `/drives/rootfs`, then start a wall-clock timer, `PUT
  /actions {InstanceStart}`, and poll the console log every 1ms until
  `CHILD_APP_RAN` appears or 10s elapses.
- Config is the same one slice 2's boot test uses: `guest-init-rootfs.ext4`,
  `/etc/cirro-init.json` pointing at `fixtures/child_app.rs`'s compiled
  binary.

## Environment

| | |
| --- | --- |
| Date | 2026-09-24 |
| Host | Linux 7.2.5-3-omarchy, x86_64 |
| CPU | 13th Gen Intel(R) Core(TM) i9-13900H |
| Firecracker | v1.17.0 |
| Guest kernel | vmlinux-6.18.48 |
| VM config | 1 vCPU, 128 MiB RAM |
| Boot args | `console=ttyS0 reboot=k panic=1 init=/init` (same as the Step 0 / M2 correctness tests — verbose kernel logging, not tuned for speed; see Caveats) |

## Results (20 runs)

| Run | ms | Run | ms |
| --- | --- | --- | --- |
| 1 | 655.989 | 11 | 672.387 |
| 2 | 613.488 | 12 | 619.177 |
| 3 | 625.466 | 13 | 673.334 |
| 4 | 664.753 | 14 | 681.787 |
| 5 | 674.401 | 15 | 672.967 |
| 6 | 617.605 | 16 | 631.314 |
| 7 | 678.035 | 17 | 629.430 |
| 8 | 662.599 | 18 | 628.342 |
| 9 | 631.176 | 19 | 677.775 |
| 10 | 633.469 | 20 | 619.551 |

| Stat | Value |
| --- | --- |
| n | 20 |
| min | 613.488 ms |
| max | 681.787 ms |
| mean | 648.152 ms |
| median | 644.729 ms |
| stdev | 24.933 ms |

All 20 runs succeeded (no timeouts, no kernel panics — the slice 1 panic-on-return
bug doesn't recur here since guest-init still never returns from `main`).

## Caveats / follow-ups

- **This almost certainly isn't dominated by guest-init's own code.** The
  console log for one boot has 262 kernel log lines, all written to a 115200
  baud emulated serial port before `init` even runs — a well-known Firecracker
  boot-time cost, and the likely reason this is ~5x Firecracker's commonly
  quoted ~125ms figure (which is measured with `quiet` and a stripped-down
  boot). We kept the verbose boot args here because the correctness tests
  (`test_guest_init_boot.sh`) rely on `grep`-ing the console for markers and
  panics; a quieter boot would change what those tests can see.
- **Low variance (stdev ≈ 4% of mean)** suggests this is a real, repeatable
  floor for *this* boot configuration, not measurement noise — but it's a
  floor for a deliberately unoptimized boot, not for the platform.
- **Useful follow-up, not done here:** re-run with `quiet loglevel=0` (or
  equivalent) once a slice needs a truer cold-boot number, to separate
  "kernel console I/O cost" from "guest-init's own mount+fork+exec cost."
  If the M6 snapshot-restore comparison ever needs the isolated
  guest-init-only cost, that's the version to reach for.
- 1 vCPU / 128 MiB is the same minimal config the correctness tests use, not
  a chosen "production" shape — resource sizing hasn't been explored.

## Reproduce

```sh
cd scripts/step0
./build_rootfs_guest_init.sh   # only if guest-init/fixture changed
./speed_test_guest_init.sh 20  # arg = run count, default 20
```
