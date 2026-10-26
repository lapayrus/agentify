# 11: Installable app and data root

**What to build:** A per-user Windows installer that needs no admin, installs the app, daemon, Caddy, and helpers, puts state in one non-roaming data root, starts the daemon at login, and refuses mismatched runtime versions.

**Blocked by:** 09 The vertical slice.

**Status:** ready-for-agent

- [ ] The NSIS installer installs per user without elevation and the app runs from the installed copy.
- [ ] All state, logs, Checkpoints, rootfs cache, runtime disks, and the discovery file live under `%LOCALAPPDATA%\Agentify\`.
- [ ] The daemon starts at login with auto-restart and works with the UI closed.
- [ ] The discovery file ACL is limited to the current user.
- [ ] API, daemon, MCP, Caddy, and adapter versions are pinned; a mismatch returns `VersionMismatch` and refuses the Backend.
- [ ] Installing over an existing version keeps Projects and state.
