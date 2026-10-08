# Public hostnames get ACME certificates over HTTP-01, through the edge

With `--acme-email`, the Node agent gets each Public hostname's certificate from an ACME CA (Let's Encrypt unless `--acme-directory` names another) and proves the hostname over HTTP-01: the CA fetches `/.well-known/acme-challenge/<token>` from the HTTP edge, which answers it for any Host while an order is under way. Hostnames that aren't public keep getting the Node CA's certificates. A Public hostname gets no certificate at all until its ACME one arrives, so clients see a failed handshake rather than a certificate they would warn about; why the order failed is in the agent's log, and it is retried after 5 minutes, doubling up to 6 hours, to stay inside the CA's limits on failed validations. The account (one per directory) and each hostname's key and chain (one file, written whole) live root-only in the state dir, and a hostname's file outlives its App so running it again reuses the certificate.

## Considered options

- **TLS-ALPN-01.** Proves the hostname on port 443 alone, but needs the TLS edge to answer a special ALPN handshake with a per-challenge certificate. HTTP-01 needed only one route in the HTTP edge, and a Node serving public hostnames serves port 80 anyway.
- **DNS-01.** The only way to get wildcard certificates, but needs credentials for each DNS provider. Out of scope for v0.1.
- **The Node CA's certificate until the ACME one arrives.** Clients would be told to distrust a certificate for a public name, and some would remember the exception. Rejected.

## Consequences

The edge must be reachable on port 80 under each Public hostname, and passing `--acme-email` agrees to the CA's terms of service. Let's Encrypt production is the default directory, so a misconfigured test Node orders real certificates; point `--acme-directory` at the staging directory to try things out. Tested against Pebble, with real HTTP-01 validation through the edge.
