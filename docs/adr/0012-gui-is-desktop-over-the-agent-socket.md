# The GUI is a desktop app over the agent socket

`cirro-gui` is a Dioxus desktop app (a webview on Linux) that the operator runs on the Node. It talks to the Node agent the same way the CLI does: HTTP+JSON over the agent's Unix socket, with the client in `cirro-proto` behind its `client` feature. We chose this over a browser console (Dioxus web or fullstack) served from the Node because a browser can't open a Unix socket. A browser console would need a TCP listener on the agent, or a proxy beside it, plus logins, sessions and CSRF protection. The socket already answers "who may do this" through its `cirro` group and peer credentials (ADR 0002), and the GUI keeps that answer.

## Considered options

- **Dioxus web or fullstack, with an HTTP API on the agent.** It would be reachable from a phone or laptop, but it is a new network-facing attack surface on a root service, with its own authentication. Rejected.
- **A separate web proxy that forwards to the socket.** It keeps the agent local, but the proxy then holds the `cirro` group's power over every VM, so it needs the same authentication work. Rejected.

## Consequences

- The GUI only works on the Node itself, for users in the socket's group. Remote use goes through SSH, where `cirro top` still runs.
- The GUI ships as its own `cirro-gui` binary, so the CLI never links webkit2gtk. It also builds in its own CI job, kept out of the workspace's default build.
- Reopen this decision before adding any TCP listener to the Node agent for a UI.
