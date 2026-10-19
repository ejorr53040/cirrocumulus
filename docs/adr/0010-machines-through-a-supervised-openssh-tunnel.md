# Machines are reached through a supervised OpenSSH tunnel

A Machine is a computer the Node agent reaches over SSH so Apps can run on it without KVM or the agent. The agent runs the system's OpenSSH `ssh`, one long-lived process per Machine, rather than speaking SSH itself. Each runs with `-F /dev/null` (none of root's own config), `BatchMode`, the agent's own Ed25519 key (`IdentitiesOnly`), and `StrictHostKeyChecking=yes` against a `known_hosts` file of the agent's that holds each Machine's pinned key under its name (`HostKeyAlias`). `ServerAlive*` notices a dead link with no traffic. The agent learns the connection is up from `ssh`'s `Authenticated to` line at `LogLevel=VERBOSE`, and why it went down from its last line on stderr. An `ssh` dies with the agent (`PR_SET_PDEATHSIG`), so a crash never leaves a connection nothing owns. A host key is pinned only by fingerprint, given with `--host-key` or confirmed by the operator after a scan; the scan asks for the Ed25519 key alone, since each key type is a pre-auth connection that OpenSSH's `PerSourcePenalties` counts against the Node.

## Considered options

- **An SSH library in the agent (`russh`).** Finer control over channels, but a large new dependency in a root process, and its own host-key and crypto code to trust instead of OpenSSH's. Rejected.
- **Require Machines to be routable and dial them directly.** No tunnel at all, but no Machine behind NAT or a firewall, and plaintext from the Edge across whatever network lies between. Rejected.
- **The Machine dials the Node (a reverse tunnel).** Works behind NAT, but needs the Node to accept SSH from Machines and Machines to hold a credential for the Node. Left for later.
- **Trust on first use without confirming.** Simpler `machine add`, but the first connection's network decides what's trusted from then on. Rejected; the operator confirms or gives the fingerprint.

## Consequences

Machines need OpenSSH (`ssh`, `ssh-keygen`, `ssh-keyscan`) on the Node; `cirro node install` warns when `ssh` is missing. Status comes from parsing `ssh`'s VERBOSE output, so a change to that line's wording would show every Machine as unreachable; the integration tests run against the real `sshd` and `ssh`. Every Machine shares the agent's key, so one key's compromise opens them all.
