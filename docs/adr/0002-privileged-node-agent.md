# A privileged Node agent owns VMs; the CLI never touches them directly

`cirro node` runs as a long-lived root service (systemd) that creates, tracks and tears down every VM and its host state (namespaces, taps, NAT, jails); `cirro run`/`ps`/`stop`/`logs` are requests to it over a local Unix socket. We chose this in M3, instead of having the CLI boot and detach VMs itself with per-command `sudo` rules, because later milestones (idle-timer parking, the edge router holding requests during wake) need an always-on owner anyway, and the per-command sudoers approach had already grown into over-broad wildcard rules.

## Consequences

VMs outlive the Node agent: restarting or upgrading it must not stop workloads. On startup the agent re-adopts VMs that are still running from its persisted state and fully removes the host state of any that died while it was down, so a crash never leaves artifacts behind. The CLI reaches the agent through a socket writable by a `cirro` group, which makes that group equivalent to control over every VM on the Node (not host root); the threat model must say so.

The startup cleanup treats every `cirro-*` namespace, veth, jail directory and cgroup on the host that falls in the agent's Node subnet and that no running VM holds as its own. That is why a host runs one Node agent: host objects are named by the low 16 bits of the VM address alone, so two agents on subnets that share those bits (say `10.77.5.0/24` and `10.78.5.0/24`) would sweep each other's live VMs. An agent also refuses a state dir that was made for a different Node subnet.
