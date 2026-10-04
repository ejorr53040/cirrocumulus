# The edge is a hyper proxy inside the Node agent, not Pingora

Every Node serves its Apps through an edge that routes by hostname, holds a request while a parked App wakes, and parks Apps that go idle. All three read or change the agent's VM table, so the edge runs in the Node agent's process and asks it where a hostname goes through a small `Router` trait (`cirro-edge`). It is built on hyper and, for TLS, tokio-rustls: the agent is already a tokio and hyper process, so the edge adds no runtime and no new HTTP stack.

## Considered options

- **Pingora, as RESEARCH.md planned.** Pingora brings its own server with its own threads, runtime, signal handling and daemon mode (`Server::run_forever`), which doesn't sit inside an existing tokio process, so the edge would be a second process talking to the agent over its socket for every wake and route lookup. Its strengths (connection pooling at CDN scale, hot reload of the proxy itself) don't matter at one Node's traffic. Rejected for v0.1.
- **The edge as its own process.** Survives an agent restart, but every request for a parked App would cross the agent's socket, and routes would need a second copy kept in step. Rejected; a restart of the agent drops in-flight edge connections, which is the same cost the agent's own socket already has.

## Consequences

The edge opens one upstream connection per request and has no pooling; that is enough for v0.1 and is the first thing to revisit if the edge's own latency shows up in `cirro bench`. WebSocket and other `Upgrade` requests are not proxied yet. If the edge outgrows hyper, the `Router` trait is the seam a Pingora-based edge would implement against.
