# Changelog

All notable changes to Cirrocumulus. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (pre-1.0: a minor version may break
things).

## [0.1.0] - Unreleased

The first release: one Node, running OCI images in jailed Firecracker
microVMs, with a terminal dashboard, park and wake, and an edge that routes
HTTP(S) to Apps by hostname.

### Added

- **One binary.** `cirro` is the CLI, the Node agent and the guest's PID 1
  (guest-init, embedded and written into each rootfs).
- **Node setup.** `cirro node install` checks for KVM and cgroup v2, fetches
  and verifies the pinned Firecracker, jailer and guest kernel, creates the
  `cirro` group, lets the Node subnet's traffic past ufw or firewalld
  (ADR 0009), and installs the Node agent as a systemd unit;
  `cirro node uninstall` reverses it, and with `--force` kills the VMs
  still running.
- **VMs.** `cirro run` boots a VM from an OCI image (`nginx:alpine`) or an
  ext4 rootfs, jailed by jailer in a network namespace of its own;
  `ps`, `logs [--follow]`, `stop [--force]` (SIGTERM to the App first),
  `rm`. VMs outlive the Node
  agent, which takes them back when it starts again.
- **Images.** `cirro image pull | ls | rm`: public `linux/amd64` images,
  built into a rootfs as the calling user, not root (ADR 0004).
- **Egress policy.** One Node-wide nftables table masquerades VMs' traffic
  and drops SMTP, VM-to-VM and LAN destinations.
- **Dashboard.** `cirro top`: live CPU, memory, disk and network use of the
  Node and each VM, at 1 s.
- **Park and wake.** `cirro park` snapshots a VM and frees its RAM;
  `cirro wake` starts it again where it was, in 43 ms p50 and 54 ms p99 for a
  256 MiB guest on the reference laptop. The guest's kernel reseeds its RNG
  on every wake (VMGenID). `cirro bench` measures boot, park and wake.
- **Apps and the edge.** `cirro run --host web.example.com --port 8080`
  gives a VM a Route; the Node agent's edge (`--http`, `--https`) proxies
  requests to it by hostname, wakes a parked App when a request arrives and
  holds the request meanwhile, and with `--idle-park <secs>` parks an App
  that has had no request for that long. A client gets 10 s to send each
  request's headers.
- **TLS.** The HTTPS edge serves certificates from a CA of the Node's own
  (`cirro node ca` prints it), and with `--acme-email` gets public
  hostnames' certificates from Let's Encrypt over HTTP-01 (ADR 0008).
- **Docs.** [Threat model](docs/threat-model.md), [security
  policy](SECURITY.md), ADRs 0001–0009, and a docs site (mdBook, `docs/`).
- **Releases.** Tagging `vX.Y.Z` builds `cirro` and publishes it with its
  SHA-256; every crate is ready for `cargo publish --workspace`.
- **Fuzzing.** cargo-fuzz targets for the OCI layer parser and guest-init's
  config ([fuzz/README.md](fuzz/README.md)).

### Known limits

- One Node: `cirro server` and `cirro node join` are not implemented yet.
- `cirro ssh` and `cirro db` are not implemented yet.
- Snapshots and the state database aren't fsynced, so a power cut can lose a
  parked VM (ADR 0006).
- No third-party audit yet, and only minutes of fuzzing
  ([fuzz/README.md](fuzz/README.md)).

[0.1.0]: https://github.com/ejorr53040/cirrocumulus/commits/main
