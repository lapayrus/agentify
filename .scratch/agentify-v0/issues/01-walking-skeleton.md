# 01: Walking skeleton

**What to build:** A runnable daemon and desktop shell with no Environments yet. The daemon starts from a cold data root, migrates its SQLite state, writes a user-only discovery file, serves `/v1/whoami`, and keeps an SSE heartbeat open. The Tauri shell shows the chosen header and an empty Project rail. The MCP shim exposes `whoami` over stdio and returns the same payload as the API.

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

- [ ] The daemon starts from a cold data root, applies migrations, and survives a restart with state intact.
- [ ] The discovery file holds a loopback port and an Agent-scoped token, readable only by the current OS user.
- [ ] `GET /v1/whoami` returns project context, scopes, daemon and API versions, and capabilities.
- [ ] The MCP `whoami` tool returns the same payload as the HTTP endpoint.
- [ ] The shell opens with the header, an empty Project rail, and a clear empty state; an unreachable daemon shows a distinct connection state.
- [ ] An API-seam test boots the daemon and asserts whoami, discovery, and persistence.
