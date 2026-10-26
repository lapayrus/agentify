# 10: Durability and isolation

**What to build:** The slice survives a daemon restart, and a second Project proves Project-to-Project isolation: no shared volumes, no shared database credentials, no hostname resolution across Projects.

**Blocked by:** 09 The vertical slice.

**Status:** ready-for-agent

- [ ] Restarting the daemon leaves the row intact and the URL serving without a manual start.
- [ ] A second Project starts alongside the first; both routes and leases are correct.
- [ ] Project A cannot read Project B's volume, authenticate to B's database, or resolve B's hostname.
- [ ] Every mutation from both Projects appears in the audit with the correct actor and target.
- [ ] Acceptance checks 5, 8, and 10 pass in the scripted MCP suite.
