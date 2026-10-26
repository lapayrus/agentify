# 08: Windows WSL Backend: exec, logs, stats, copy

**What to build:** The operational surface of the real Windows Backend: streaming exec with cancellation, log following, resource statistics, and daemon-owned file copy.

**Blocked by:** 07 Windows WSL Backend: lifecycle.

**Status:** ready-for-agent

- [ ] `exec` streams stdout and stderr, returns the exit code, and terminates the process group on cancellation.
- [ ] `logs` supports tail, since, and follow, and survives a slow consumer without blocking the workload.
- [ ] `stats` returns environment-level CPU, memory, and disk, with per-Resource figures where the substrate allows.
- [ ] `copyIn` and `copyOut` accept only daemon-owned paths and reject traversal.
- [ ] Capability gaps return typed `Unsupported` rather than raw stderr.
- [ ] The full Backend conformance suite is green against the real distro.
