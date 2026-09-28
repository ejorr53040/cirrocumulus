# Each VM runs in its own network namespace

Every VM's Firecracker process is jailed into a dedicated network namespace (jailer `--netns`), with its tap inside that namespace and a veth pair linking it to the Node, where routing and NAT happen. We chose this over sharing the Node's default namespace (what M3's first jailer+tap slices did) because the published threat model promises net-namespace containment of the VMM, and "hard isolation by default" is principle #1; retrofitting it after run, park/wake and the edge router exist on host networking would be a rewrite.

## Consequences

Each VM costs an extra namespace and veth pair, and teardown has more pieces that must be removed to avoid leaving artifacts on the Node. Raw tap traffic never reaches the Node's own firewall chains.
