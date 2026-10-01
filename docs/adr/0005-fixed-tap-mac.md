# Every VM's tap has the same MAC, so a woken guest can reach out at once

A woken VM gets a new network namespace and a new tap, but its guest resumes with the ARP cache it was parked with, which maps its gateway (the tap's address, fixed by ADR 0003) to the old tap's MAC. Frames to that MAC are dropped by the new tap. Traffic coming in hides this, because the namespace's ARP request for the guest updates the guest's entry for its gateway on the way, but a guest that speaks first gets nothing out: in our test a woken guest dialling the internet every 100 ms failed every dial for over 10 s. So, extending ADR 0003's "network for clones" pattern from addresses to links, the tap in every namespace gets the same locally administered MAC, `06:00:ac:10:00:01`, and the first dial out after a wake succeeds once the dial in flight at the park has timed out (under 2 s; 0.1-0.6 s measured).

## Considered options

- **Have guest-init send a gratuitous ARP, or flush its neighbour table, after a restore.** It needs a restore hook in the guest that runs before the app's traffic, and leaves a window where the guest talks to the wrong MAC. Rejected in favour of making the host side match what the snapshot expects.

## Consequences

Every snapshot depends on this MAC: changing it later means waking old snapshots with the old one. Only the tap is pinned. Its link joins it to its own guest and nothing else, so the shared MAC never meets another one on the same link.
