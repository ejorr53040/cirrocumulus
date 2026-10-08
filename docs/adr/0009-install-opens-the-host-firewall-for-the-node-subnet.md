# Install lets the Node subnet's traffic past the host firewall

`cirro node install` adds one rule to the host's own firewall so VMs reach the internet on hosts whose firewall drops forwarded traffic by default (ufw's `DEFAULT_FORWARD_POLICY="DROP"`, firewalld's zones), and `cirro node uninstall` removes it. With ufw installed, active or not, it adds `ufw route allow in on cirro-+ from <subnet>`, matching the VMs' veths and no egress interface, so it holds when the default route moves. With firewalld, the subnet becomes a source of the `trusted` zone: at runtime and saved while firewalld runs, saved alone (`firewall-offline-cmd`) while it doesn't. The egress policy's own drops (SMTP, other VMs, the LAN, the Node) still apply, since an nftables drop is final across tables; the rule only stops the host firewall dropping the rest. If a firewall that is there refuses the rule, install fails rather than leaving a Node whose VMs silently can't reach out.

## Considered options

- **Tell the operator to add the rule.** Every fresh Node on a firewalled host would come up with VMs that have no internet and no error saying why. Rejected.
- **Accept forwarded traffic in the egress table.** An nftables accept isn't final across tables, so ufw's or firewalld's own chains would still drop it. Rejected.
- **Add the rule only while the firewall is active.** Turning ufw on later would cut VMs off. Rejected.

## Consequences

Install changes host state outside its own state dir, so uninstall has one more thing to undo, best-effort like the rest. Other host firewalls (raw iptables rules, nftables policies of the host's own) are left alone and are the operator's to open. Tested against ufw; the firewalld path is not exercised in CI.
