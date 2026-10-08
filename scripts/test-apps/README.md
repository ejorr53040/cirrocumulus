# Test Apps

Five tiny Apps for exercising a Node by hand. Each one is an ext4 rootfs:
Alpine's busybox, guest-init as `/init`, and an `/entrypoint` script.

| App | What it's for | Knobs (`-e KEY=VALUE`) |
| --- | --- | --- |
| `hello-http` | HTTP on :80, a status page, park and wake, the edge | `PORT`, `START_DELAY` |
| `cpu-burn` | CPU in `cirro top` | `WORKERS` (default 1) |
| `mem-hog` | Memory in `cirro top`, the guest's OOM killer | `TARGET_MB` (128), `STEP_MB` (16), `STEP_SECS` (2) |
| `crash` | An App that dies: exit codes and signals | `CRASH_AFTER` (5), `EXIT_CODE` (3), `SIGNAL` (`SEGV`, `ABRT`, …) |
| `net-probe` | The Node's egress policy, checked from inside a VM | `PROBES`, `TIMEOUT` (3) |

`hello-http`'s `/cgi-bin/status` prints the guest's `boot_id`, uptime and a hit
count. After a park and wake, the `boot_id` is the same and the hits carry on,
which shows the VM was resumed, not rebooted.

`net-probe` prints a table to the console at boot and serves a fresh one at
`/cgi-bin/probe`. `blocked` means no answer before the timeout, which is how
the Node's drop rules look. `refused` means something answered with a reset,
so it was reachable.

## Preparing an App for Cirrocumulus

`cirro run` takes either of two things.

**An OCI image** from a public registry (`nginx:alpine`, `ghcr.io/o/app:tag`).
The CLI pulls it, builds a rootfs as you, puts guest-init in as `/init`, and
uses the image's entrypoint, env and workdir. The image must be:

- public (there's no registry login yet)
- `linux/amd64`
- made of gzip or uncompressed layers (not zstd)

Push your image somewhere public, then `cirro run --name x <ref>`. See
[docs/rootfs-from-an-image.md](../../docs/rootfs-from-an-image.md).

**An ext4 rootfs you built yourself**, which is what these Apps are. It needs:

1. **guest-init at `/init`**, built static for musl:
   `cargo build --release --target x86_64-unknown-linux-musl -p cirro-guest-init --bin guest-init`.
   The kernel runs it as PID 1. It mounts `/proc`, `/sys` and `/dev`, takes
   the command over vsock, and execs it.
2. **Empty `/proc`, `/sys` and `/dev`** for guest-init to mount onto, and a
   writable `/tmp`.
3. **Your App and everything it needs to run**: interpreter, libraries,
   `/etc/resolv.conf` if it resolves names. A rootfs gets no resolv.conf from
   `cirro`, unlike a pulled image.
4. **A command**: a rootfs has no default, so pass it after `--`. With no
   `PATH` in `-e`, guest-init sets a standard one and `HOME=/`.
5. **An ext4 image**: `mkfs.ext4 -d <tree> <file>.ext4` (e2fsprogs 1.47.1 or
   newer). Each VM gets its own copy, so writes don't leak between VMs.

`build.sh` does all of this without root or Docker. Alpine's static `apk`
installs packages into a plain directory. An unprivileged user namespace
lets `busybox --install` run in a chroot. Then `mkfs.ext4 -d` packs the tree.

**To add an App**, make `apps/<name>/entrypoint` (plus any other files, which
are copied to the rootfs root) and run `./build.sh <name>`. For more than
busybox, add packages to the `apk add` line and delete `.build/base` so it's
rebuilt.

## Commands

### Once per host (root)

```sh
cargo build --release -p cirrocumulus
sudo install -m 755 target/release/cirro /usr/local/bin/cirro
sudo cirro node install --http 0.0.0.0:80   # --http turns on the edge
sudo usermod -aG cirro "$USER"              # then log in again
```

`cirro node install` also lets the Node subnet's traffic past ufw or
firewalld.

If `net-probe` shows `internet-https blocked`, check the agent's log for
`no IPv4 default route`: the agent picks the egress interface once, at
startup, so one started before the network came up never masquerades VM
traffic. `sudo systemctl restart cirro` fixes it; VMs outlive the restart.

### Build

```sh
scripts/test-apps/build.sh               # every App
scripts/test-apps/build.sh hello-http    # just one
cd scripts/test-apps/.build && ls *.ext4
```

`cirro run` prints each VM's address. The `10.77.0.x` addresses below are
examples; use the one it prints.

### hello-http: run, curl, park and wake

```sh
cirro run --name web ./hello-http.ext4 -- /entrypoint
curl http://10.77.0.2/
curl http://10.77.0.2/cgi-bin/status
cirro park web                    # snapshot to disk, free the RAM
cirro wake web                    # same boot_id, hits carry on
curl http://10.77.0.2/cgi-bin/status
cirro logs web
```

### As an App behind the edge

Needs a Node installed with `--http`.

```sh
cirro run --name app --host web.localhost --port 80 --idle-park 30 \
    ./hello-http.ext4 -- /entrypoint
curl -H 'Host: web.localhost' http://127.0.0.1/cgi-bin/status
# 30 s without a request and it parks; the next request wakes it.
# START_DELAY makes the edge hold a request while the App comes up:
cirro run --name slow --host slow.localhost --port 80 -e START_DELAY=5 \
    ./hello-http.ext4 -- /entrypoint
```

### cpu-burn and mem-hog in the dashboard

```sh
cirro run --name burn --vcpus 2 -e WORKERS=2 ./cpu-burn.ext4 -- /entrypoint
cirro run --name mem --mem 256M -e TARGET_MB=192 ./mem-hog.ext4 -- /entrypoint
cirro top
cirro run --name oom --mem 128M -e TARGET_MB=512 ./mem-hog.ext4 -- /entrypoint
cirro logs oom                    # the guest kernel's OOM killer at work
```

### crash

```sh
cirro run --name boom -e CRASH_AFTER=2 -e EXIT_CODE=7 ./crash.ext4 -- /entrypoint
cirro run --name segv -e SIGNAL=SEGV ./crash.ext4 -- /entrypoint
cirro ps                          # gone from the list once Ended
cirro logs boom                   # "crash: exiting 7", then the VMM stopping
```

### net-probe

```sh
cirro run --name probe ./net-probe.ext4 -- /entrypoint
sleep 25; cirro logs probe        # the report printed at boot
curl http://10.77.0.2/cgi-bin/probe
```

Custom probes, one `label host port expected` per line:

```sh
cirro run --name probe2 -e 'PROBES=router 192.168.0.1 80 blocked
web example.com 443 connected' ./net-probe.ext4 -- /entrypoint
```

### Bench and clean up

```sh
cirro bench --runs 10 ./hello-http.ext4 -- /entrypoint
cirro stop web
cirro rm web                      # an Ended VM's record and log, or a parked VM's snapshot
```
