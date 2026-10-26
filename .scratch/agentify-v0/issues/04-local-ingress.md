# 04: Local ingress and trusted hostnames

**What to build:** Caddy, owned by the daemon, routes every declared Resource port to `<resource>.<project>.localhost` over loopback. HTTPS works with a locally trusted certificate; HTTP works when trust is refused. A stopped Environment keeps its route and returns a clear 503.

**Blocked by:** 03 Environment lifecycle on the fake Backend.

**Status:** ready-for-agent

- [ ] The daemon starts and configures Caddy through its admin API on a random loopback port, one route per Resource port.
- [ ] `https://web.hello.localhost` reaches the fake upstream; a running app is reachable without any hosts-file edit.
- [ ] Windows trust installs into the per-user root without elevation and enables Firefox enterprise roots; refusing trust leaves HTTP serving the same app.
- [ ] A stopped Environment returns a daemon-authored 503; only removing the Environment deletes the route.
- [ ] Port 80 or 443 already in use fails with `PortUnavailable` and names the holder.
- [ ] SSE and WebSocket traffic pass through; `X-Forwarded-For`, `X-Forwarded-Proto`, and `X-Forwarded-Host` are set.
