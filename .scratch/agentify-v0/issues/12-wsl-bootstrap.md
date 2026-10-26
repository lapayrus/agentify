# 12: WSL bootstrap and first run

**What to build:** A first-run flow on a clean Windows machine: detect WSL and virtualization without elevation, offer one explicit UAC step, resume after reboot, download the pinned rootfs, install trust, and land in a working app.

**Blocked by:** 11 Installable app and data root.

**Status:** ready-for-agent

- [ ] Detection reports WSL and virtualization state without elevation and without gating on SLAT.
- [ ] A machine without WSL gets an explicit, consenting UAC flow that enables WSL, schedules a RunOnce resume, and explains the reboot.
- [ ] After reboot the app resumes, downloads and caches the pinned rootfs, and starts the daemon and Caddy.
- [ ] Trust installs with no elevation on Windows and Firefox enterprise roots are enabled; refusing trust falls back to HTTP with a clear banner.
- [ ] A clean machine reaches a working `hello-pg` slice without manual commands.
- [ ] Rerunning first run is idempotent and does not duplicate rootfs downloads or trust entries.
