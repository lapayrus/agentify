# 13: Updates and uninstall

**What to build:** Signed updates that never interrupt an operation, can roll back, and adopt running runtimes; uninstall that removes the app, daemon, Caddy, and trusted root but leaves Projects unless removal is explicitly confirmed.

**Blocked by:** 12 WSL bootstrap and first run.

**Status:** ready-for-agent

- [ ] Updates are signature-verified, stable by default with an opt-in beta, and refuse while any operation is in flight.
- [ ] An update applies on restart, leaves running runtimes alive, and the new daemon adopts them; failed adoption marks Projects degraded.
- [ ] A daemon that fails its health check after an update triggers automatic rollback to the previous version.
- [ ] Uninstall removes the app, sidecar, login item, Caddy data, and trusted root, and leaves Projects and state.
- [ ] An explicit, confirmed removal deletes runtimes, volumes, Checkpoints, and the state database, and verifies the result.
- [ ] Substrate image updates are separate from app updates and gated by a compatibility check.
