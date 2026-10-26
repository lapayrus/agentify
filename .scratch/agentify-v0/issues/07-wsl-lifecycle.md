# 07: Windows WSL Backend: lifecycle

**What to build:** A real Windows Backend that provisions one WSL distro per Project, runs a foreground supervisor, binds leased loopback ports, reports status, stops without losing data, and removes the runtime with verification.

**Blocked by:** 03 Environment lifecycle on the fake Backend.

**Status:** ready-for-agent

- [ ] The Ubuntu base rootfs is downloaded once with a pinned digest and cached; no admin rights are needed after WSL is enabled.
- [ ] `start_environment` imports a per-Project distro, starts the supervisor and container engine, and binds each leased port on `127.0.0.1`.
- [ ] `status` reports real Resource state; `stop_environment` stops workloads and keeps the distro and volumes.
- [ ] `remove` unregisters the distro, deletes its volumes, and verifies that no distro or lease remains.
- [ ] The adapter reports its capability descriptor; unsupported operations return `Unsupported`.
- [ ] The lifecycle half of the Backend conformance suite is green against the real distro.
