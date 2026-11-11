# Security review, 2026-11-11: sandbox escapes and networking

A manual source review of the `untangle` branch (230ed83), looking for two
things: ways out of a VM's sandbox, and holes in how the Node's networking
is put together. It covers `cirro-node` (VM lifecycle, egress policy, agent
API, Machines), `cirro-edge`, `cirro-image` and `cirro-proto`. guest-init was
read only as the receiver of its config. Firecracker and jailer themselves
are upstream and out of scope.

Each finding says how it was confirmed. "Test" means a test in
`crates/cirro/tests/node_agent.rs` or `crates/cirro-image` failed against
the old code on a real Firecracker VM and passes now. "Reading" means the
code says so and nothing was run to prove it.

Severities are relative to the [threat model](threat-model.md). Several
findings start from a VMM that guest code has already taken over. That is the
case the jailer, namespace and egress layers exist for, so a hole there is
rated by what it adds to such an escape.

## Summary

| # | Severity | Finding | Status | Confirmed by |
| --- | --- | --- | --- | --- |
| 1 | High | Wake chowns whatever a taken-over VMM left in place of its snapshot files | Fixed | Test |
| 2 | Medium | The Node takes IPv6 from VMs' veths, past the IPv4-only egress policy | Fixed | Test (the setting), reading (the reach) |
| 3 | Medium | The edge's 503s show internet clients the agent's own errors and the guest's console | Fixed | Test |
| 4 | Low | The agent buffers a request body of any size | Fixed | Test |
| 5 | Low | The agent buffers a Firecracker API error of any size | Fixed | Reading |
| 6 | Low | A registry's index entry is used as a cache file name unchecked | Fixed | Test |
| 7 | Low | The docs claim a PID namespace the agent never asks jailer for | Docs fixed | Reading |
| 8 | Medium | Console logs have no size limit: a guest can fill the state dir's disk | Open | Reading |
| 9 | Low | A taken-over VMM can point the agent's Firecracker API calls at another VM | Open | Reading |
| 10 | Low | Two agents whose subnets share their low 16 bits tear down each other's VMs | Open | Reading |
| 11 | Info | Turning on `ip_forward` makes the Node a router for its other interfaces | Open | Reading |

## Fixed

### 1. Wake chowns whatever a taken-over VMM left in place of its snapshot files (High)

A jail's root directory belongs to its VMM's uid, so the VMM can rename,
remove and create files in it. A park kills the VMM and then renames
`park.vmstate`, `park.mem` and `rootfs.ext4` out of the jail into the parked
dir. A wake renames them into the new jail and then ran
`std::os::unix::fs::chown` on each, which follows symlinks, as root, outside
the chroot.

A VMM that guest code had taken over could therefore, before being parked:

- replace a snapshot file with a symlink to any path on the Node. The next
  wake chowned that path's target to the new VM's uid. The wake then fails to
  load, but the chown has happened.
- rename `/vmlinux` over a snapshot file. The kernel is hard-linked into
  every jail from the Node's one image, so the wake chowned the shared inode
  to the VM's uid, and the woken VMM could then write the kernel every later
  VM on the Node boots. This variant was read, not run.

Idle park makes the trigger automatic for an App with `idle_park_secs`: the
next request through the edge wakes it.

Fix (`vm.rs`, `wake_steps`): before the snapshot leaves the root-only parked
dir, each file must be a regular file with a link count of 1, and the chown
is now `lchown`. A snapshot that fails the check is refused with an error
naming the file; `cirro rm` discards it.

Test: `wake_refuses_a_snapshot_file_that_is_a_link_and_chowns_nothing`
(symlink and hard link).

### 2. The Node takes IPv6 from VMs' veths (Medium)

The egress policy is one `ip`-family nftables table. Its input chain drops
new IPv4 connections from VM veths to the Node. Nothing covered IPv6: the
Node's end of each veth came up with IPv6 on and a link-local address.

A guest can't use this by itself, since its namespace doesn't forward IPv6.
A process inside the namespace can: a taken-over VMM could open a socket to
the veth's link-local address and reach every service the Node listens for
on `::` (sshd, the edge, anything else), which the IPv4 rule exists to
prevent.

Fix (`vm.rs`, `host_steps`): the agent writes `1` to
`net.ipv6.conf.<veth>.disable_ipv6` right after creating the veth, before
the namespace's end comes up, so the kernel drops every IPv6 packet arriving
on it. A Node with no IPv6 at all has nothing to write.

Test: `the_node_takes_no_ipv6_from_a_vms_veth` checks the setting and that
the veth has no IPv6 address. The reach itself (connecting from inside a
namespace) wasn't demonstrated; it needs root in the namespace.

### 3. The edge's 503s show clients the agent's own errors (Medium)

When a request woke a parked App and the wake failed, the edge answered 503
with the agent's whole error. That error is written for `cirro wake`: it
carries the Firecracker API's response, and the last ten lines of the
guest's console. The same text was kept and served to every later request
for that App. For a Machine-backed App whose Machine was down, the 503
carried `ssh`'s last line, which names the Machine's address and port.

The edge's clients are anyone on the internet. A console tail can hold
whatever the App printed.

Fix (`agent/edge.rs`, `agent.rs`, `agent/machine.rs`): a wake that fails with
a conflict (the App's state, such as "its snapshot is gone") still answers
in the agent's words. Any other failure answers ``App "x" failed to wake;
`cirro logs x` may say why``. A down Machine's 503 names the Machine and no
more. `cirro wake`, `cirro machine ls` and the agent's log keep the detail.

Test: `a_503_for_a_failed_wake_keeps_the_nodes_error_from_the_client`.

### 4. The agent buffers a request body of any size (Low)

`read_json` collected the whole body before parsing it, so one request on
the socket could take all of the root agent's memory. The socket's callers
are `cirro` group members, who are trusted with the VMs but not meant to be
able to take the agent down.

Fix (`agent/api.rs`): bodies are capped at 1 MiB; a larger one gets a 413.

Test: `the_agent_refuses_a_request_body_too_big_to_be_a_request`.

### 5. The agent buffers a Firecracker API error of any size (Low)

The same shape on the other side: the Firecracker client collected a non-2xx
response's whole body. The API socket is in the jail and owned by the VMM, so
a taken-over VMM could answer a stop or a park with an error that never ends.

Fix (`firecracker.rs`): the error body is capped at 64 KiB. No test: the cap
is one wrapper around the read.

### 6. A registry's index entry is used as a cache file name unchecked (Low)

`cirro image pull` of a multi-platform image takes the `linux/amd64` entry's
`digest` string from the registry's index and looks for
`<cache>/<digest>.json` before anything validates it. An entry of
`../outside` made the CLI read `<cache>/../outside.json` and, had a matching
`.ext4` been there, report that file as the pulled image's rootfs. A hostile
registry could so point `cirro run` at a file elsewhere in the user's home.

Fix (`cirro-image/src/cache.rs`): an entry that isn't `sha256:` followed by
64 hex digits is refused.

Test: `an_index_entry_that_isnt_a_digest_is_refused`.

### 7. The docs claim a PID namespace the agent never asks for (Low)

`docs/threat-model.md` and `docs/firecracker-with-jailer.md` said jailer puts
the VMM in new mount and PID namespaces. jailer makes a PID namespace only
with `--new-pid-ns`, which the agent doesn't pass: that flag makes jailer
fork, and the agent identifies each VMM by the pid it spawned.

An escaped VMM therefore shares the Node's pid space. It has no `/proc` in
its chroot to list pids, and may signal only its own uid, which no other
process has, so the practical loss is small. The docs now say what is there.
Passing `--new-pid-ns` would need the agent to find the VMM's pid another way
(jailer writes it to a file in the jail) and is left as a design choice.

## Open

### 8. Console logs have no size limit (Medium)

Firecracker writes the guest's serial console straight to a file in the
state dir, opened by the agent and handed over as stdout. Nothing caps it. A
guest that prints forever fills the filesystem that also holds the state
database, every other VM's log, and the parked snapshots. The threat model
already says nothing caps snapshot disk; this is the same gap, reachable by
any guest with no escape needed.

Not fixed because each fix is a design choice: jailer's
`--resource-limit fsize=` would also cap the snapshot memory file and the
rootfs, and piping the console through the agent ends with the agent, which
VMs are meant to outlive (ADR 0002). It belongs with M11's quotas.

### 9. A taken-over VMM can point the agent's API calls at another VM (Low)

The agent reaches a VM's Firecracker at `<jail>/root/run/firecracker.socket`
by path, as root, from outside the chroot. The VMM owns `<jail>/root`, so a
taken-over VMM could replace `run` with a symlink to another jail's `run`
directory (the state dir's path is guessable). A later stop or park of the
first VM would then send Ctrl-Alt-Del, or pause and snapshot, to the second.
The second VM's snapshot lands in its own jail, so nothing is disclosed; the
cost is a stopped or paused neighbour.

A fix needs the connect to refuse symlinks under the jail root
(`openat2` with `RESOLVE_BENEATH`, then connecting through `/proc/self/fd`).

### 10. Two agents whose subnets share their low 16 bits collide (Low)

Every host object a VM owns (namespace, veth, cgroup, uid) is named from the
low 16 bits of its VM address. Two agents on one host with, say,
`10.77.0.0/24` and `10.78.0.0/24` would both name their first VM
`cirro-0002`. The second agent's `ip netns add` fails, and its unwind, which
registers every undo step before the first step runs, then deletes the first
agent's namespace and veth. The tests avoid this by giving every agent its
own third octet. Nothing stops an operator.

A fix is for a starting agent to refuse a subnet whose host ids overlap a
subnet already in the `node_subnets` set.

### 11. Turning on `ip_forward` makes the Node a router (Info)

The agent sets `net.ipv4.ip_forward=1` for the whole host, and its forward
chain accepts by default. A Node with two other interfaces (a LAN and a
VPN, say) starts forwarding between them unless a host firewall forbids it.
Docker makes the same trade and sets the forward policy to drop. Worth a
line in the install docs.

## Checked and found sound

- **Guest to Node over IPv4.** Anything a guest sends to an address the Node
  holds (the Node address, its LAN or public address, a bridge's) arrives on
  the input hook from a `cirro-*` interface and is dropped as a new
  connection. `route_localnet` is off, so 127.0.0.1 is unreachable.
- **Forged sources.** Dropped in the VM's namespace and again in the Node's
  table, before the rules that key on source address.
- **The rootfs open.** Done by a helper that has already dropped to the
  caller's uid and gid, with `O_NOFOLLOW`, a regular-file check on the open
  fd, and the fd passed back; no path is opened twice.
- **Request validation.** Names, routes, resource bounds, the command, env
  and workdir are checked by the agent, not only the CLI. No request field
  reaches a shell, the kernel command line or an nftables script.
- **SSH targets.** Each part is restricted to name characters and can't start
  with `-`; `ssh` runs with `-F /dev/null`, a pinned host key and `--`.
- **The edge.** TLS connections are held to their SNI hostname (421
  otherwise), hop-by-hop headers are stripped both ways, `X-Forwarded-For` is
  replaced, and upstreams are only VM addresses and the Node's loopback.
- **Image layers.** Never unpacked onto the host: paths are normalised and
  refused if they leave the root, and the tree goes to `mke2fs` as a tar, in
  the unprivileged CLI.
- **Teardown.** `remove_dir_all` on a jail doesn't follow symlinks, and an
  adopted VMM is killed through its cgroup, never a bare pid.
- **Secrets on disk.** The SSH key, CA key, ACME keys and parked dir are
  root-only; console logs are `root:cirro` 0640.

## Not covered

Fuzzing, dynamic testing of a real VMM escape, the edge under load,
dependency advisories (CI's `cargo deny` covers those), and the `scripts/`
tree, which the 2026-09-28 review covered.
