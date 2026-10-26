# Define the ingress contract

Type: grilling
Status: resolved
Blocked by: 03

## Question

How does a port become a hostname?

Decide port discovery (manifest-declared versus detected from the runtime), hostname allocation (`<resource>.<project>.localhost`), Caddy config generation and reload semantics through the admin API, and how the internal CA is trusted on Windows and macOS.

Cover WebSocket and SSE support, behaviour when two Projects claim the same hostname or port, and what an Agent sees when a route is unhealthy. State whether an Environment can be reached by IP as a fallback.

On Windows, distros share one network namespace, so every published hostname must map to a port leased from the daemon's machine-wide registry rather than a fixed per-Project port.

Trust installation follows ticket 14: Windows writes the per-user root with no elevation; macOS sets `install_trust false` and uses a user-domain `security add-trusted-cert`; Firefox requires `security.enterprise_roots.enabled`.

## Answer

Routes are `<resource>.<project>.localhost`, HTTP(S) only, loopback-only, served by one Caddy instance the daemon owns. Ports come from the manifest; there is no port detection.

- TLS: both listeners. HTTPS is primary; HTTP redirects when the CA root is trusted and serves directly when the user refused trust, since localhost HTTP is still a secure context.
- Route lifecycle: a route exists for as long as the Environment does. Running means `reverse_proxy 127.0.0.1:<lease>`; stopped means a daemon-authored 503 ("Environment stopped"); only removing the Environment deletes the route. An IP fallback is not exposed.
- Upstream: the daemon owns the lease and the Backend binds `127.0.0.1:<lease>`. Verified on Windows: a 127.0.0.1-bound guest port is reachable from the host at `localhost:<lease>`.
- Unhealthy apps: Caddy passes the app's response through untouched, including 5xx. Health lives in `status`, not in the proxy.
- Control: Caddy's admin API on a random loopback port, one `@id` per route, granular updates, full `/load` only at startup and recovery. Same-user processes are inside the trust boundary; ticket 12 records that.
- Port conflicts: bind 80 and 443. If either is unavailable, fail with `PortUnavailable` and name the holder. No silent port change.
- Route template: set `X-Forwarded-For`, `X-Forwarded-Proto`, and `X-Forwarded-Host`; disable response buffering for SSE (`flush_interval -1`); leave WebSocket handling at Caddy's default; no artificial timeouts in v0.
- Duplicate hostnames are rejected at manifest parse (ticket 02), so Caddy never sees a conflict.
- Out of scope for v0: custom domains, LAN exposure, and raw TCP routes.
