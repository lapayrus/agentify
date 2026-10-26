# 03: Environment lifecycle on the fake Backend

**What to build:** A scripted fake Backend that implements the contract, so the whole Environment lifecycle works deterministically before any real substrate exists. Start, stop, status, port leases, health, logs, exec, and stats all run end to end through the API, MCP, and UI.

**Blocked by:** 02 Projects from a Manifest.

**Status:** ready-for-agent

- [ ] The Backend contract exists as a substitutable interface with a capability descriptor, typed errors, and a fake implementation.
- [ ] `start_environment` is idempotent and convergent; `stop_environment` keeps the runtime; status reports `starting`, `running`, `degraded`, `stopped`, and `failed`.
- [ ] Port leases are allocated by the daemon, released on stop, and reacquired on start; a lease conflict returns `PortUnavailable`.
- [ ] Health is decided by the daemon through leased ports, not by the Backend.
- [ ] `run_command` streams stdout/stderr and returns an exit code; `read_logs` supports tail and since; `get_stats` returns environment-level figures.
- [ ] The UI shows Start/Stop, the status bar, and the Logs tab; MCP exposes the matching tools; `Idempotency-Key` replays the original response.
- [ ] A mutation that exceeds its timeout returns the operation id so the caller can wait.
