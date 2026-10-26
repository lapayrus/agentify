# Agent-native local environments

## Destination

A buildable v0 spec plus the first vertical slice: one Project (a Node app and Postgres) running inside a per-project isolated runtime on Windows, created and driven end to end by an Agent through the local API and MCP server, and by a human through the desktop UI, reachable at a `*.localhost` hostname, with Checkpoint, reset, and teardown. Reaching the end of this map means nothing is left to decide before implementation starts.

## Notes

- Platform order: Windows 11 first, macOS second, Linux follows from the container backend.
- Isolation: one runtime per Project. macOS gets a VM boundary; Windows WSL distros share one kernel and network namespace, so Windows is filesystem, process, and cgroup isolation plus a daemon-owned port registry.
- Substrate: per-project WSL2 distro (`wsl --import`) on Windows; Apple `containerization` on macOS 26+/Apple silicon with a Lima fallback. Ship only Apache-2.0/MIT components.
- Control plane: local Rust daemon on loopback or a named pipe. Tauri UI, CLI, and MCP server are thin clients of the same API.
- Manifest: `agentify.yaml` is the source of truth; `devcontainer.json` and Compose are import/export adapters.
- Ingress: one Caddy instance driven through its admin API; `*.localhost` plus Caddy's internal CA.
- Capacity: Apple container keeps roughly 1 GiB resident per running container and never returns freed guest memory; a WSL distro costs ~76 MiB disk and ~450 ms start. Autosuspend is a v0 concern, not a later one.
- Every session should consult the `grilling` and `domain-modeling` skills.
- Tracker: local markdown (`docs/agents/issue-tracker.md`).
- Spec: [spec.md](../agentify-v0/spec.md) — published with `ready-for-agent`.
- Implementation tickets: [`agentify-v0/issues/`](../agentify-v0/issues/).

## Decisions so far

- [One VM or distro per Project is the isolation boundary](../../docs/adr/0001-per-project-isolation-boundary.md): one runtime per Project; Windows is namespace-level, macOS is VM-level.
- [The Project manifest is the source of truth](../../docs/adr/0002-manifest-is-source-of-truth.md): `agentify.yaml` first; devcontainer and Compose are adapters.
- [One local daemon is the only control plane](../../docs/adr/0003-local-daemon-is-the-only-control-plane.md): loopback API, scoped tokens, Approvals, audit log.
- [Drop Team and Server from the domain model](../../docs/adr/0004-drop-team-and-server.md): Project → Environment → Resource, with Backend replacing Destination.
- [Rust daemon and Tauri shell](../../docs/adr/0005-rust-daemon-tauri-shell.md): one Rust workspace, OS webview UI.
- [Research: Windows per-Project WSL provisioning](issues/08-windows-wsl-provisioning.md): import works without admin, ~76 MiB and ~450 ms per distro, but distros share kernel and netns; port collisions are structural.
- [Research: macOS container backend](issues/09-macos-container-backend.md): Apple container on macOS 26+/Apple silicon, Lima on 14–15, Intel out; drive both through CLI+JSON with capability descriptors.
- [Decide the Windows isolation boundary](issues/13-windows-isolation-boundary.md): namespace-level isolation in v0 — distro, Docker network, cgroups, and port registry; Hyper-V VM-grade deferred.
- [Define the first vertical slice](issues/01-vertical-slice.md): agent-first Node 22 + Postgres on Windows; trusted `web.<project>.localhost`; ten-check definition of done covering reset, Checkpoint, teardown, and Approvals.
- [Define the Project manifest v0](issues/02-project-manifest-v0.md): strict `version`/`project`/`resources`; named secrets with `${secret:name}` references; app image XOR build; per-port hostnames; daemon generates runtime, volume, route, lease, and secret values.
- [Research: trust a local CA without admin](issues/14-local-ca-trust.md): Windows uses the per-user root with no elevation; macOS needs one user prompt because `caddy trust` is admin-only; Firefox needs enterprise-roots enabled.
- [Define the Backend contract](issues/03-backend-contract.md): environment-scoped lifecycle (`start`/`stop`/`remove`), exec/logs/copy/stats, capability descriptor, typed errors; the daemon owns port leases, health, and convergence.
- [Define the ingress contract](issues/04-ingress-contract.md): `<resource>.<project>.localhost`, loopback-only, HTTPS primary with HTTP fallback, stable routes with a daemon-authored 503 when stopped, Caddy admin API, fail on port conflict.
- [Define secrets and environment handling](issues/05-secrets-and-env.md): OS keyring, Project-scoped, runtime env vars only; API never returns plaintext and Project isolation plus audit is the enforceable boundary.
- [Define data lifecycle](issues/06-data-lifecycle.md): Environment-owned volumes, destructive `remove`, host-side Checkpoint archives, Approval-gated restore and reset, orphan runtimes reported not auto-deleted.
- [Define the Agent API and MCP surface v0](issues/07-agent-api-and-mcp.md): loopback REST `/v1` with SSE and WebSocket, twenty verb-first MCP tools, coarse scopes, single-use parameter-bound Approvals, problem+json codes, append-only audit.
- [Define the desktop UI v0](issues/10-desktop-ui-v0.md): light shell — B's header over A's rail/detail/side; project rail with cost, resource table, logs, Approval inbox, and activity feed; no editor, terminal, or secret UI.
- [Define distribution, bootstrap, and updates](issues/11-distribution-and-updates.md): signed per-user NSIS, automated UAC WSL bootstrap, notarized Apple-silicon DMG, pinned downloads, login-item daemon, signed updates with rollback, non-destructive uninstall.
- [Define the security threat model](issues/12-security-threat-model.md): same-user processes are inside the boundary; OS-auth Approvals, expiring Project tokens, no host mounts or privileged containers, hash-chained audit, 20-Project cap; no kernel isolation claimed on Windows.

## Not yet specified

- Checkpoint encryption at rest.
- Manifest editor, interactive terminal, and secret-management UI.
- Source editing and sync between host and runtime (v0 creates from a bundled fixture).
- `keepVolumes` (preserving data across runtime removal) through export and import.
- App-level persistent volumes (uploads) and their backup story.
- Secret rotation, per engine (for Postgres, `ALTER USER` plus a Resource restart).
- Build-time secrets and private registry authentication.
- Multi-Environment semantics beyond `dev` (test, preview) and whether Environments share or fork data.
- Template catalog governance: which services ship, versioning and pinning, supply-chain trust.
- Resource budgets and scheduling when many Projects run at once: RAM and CPU limits, autosuspend policy.
- Import from existing `docker-compose.yml` and devcontainer setups, including what a lossy import warns about.
- Update strategy for the substrates themselves (WSL base rootfs, Lima, Apple containerization) and for running Projects during an app update.
- Portability: exporting and restoring a Project on another machine.
- Concurrent collaboration on one Project: multiple Agents and humans, leases and locking.
- Observability: log retention, metrics, and what an Agent may query.

## Out of scope

- Multi-tenant teams, users, roles, invitations, and billing.
- Remote servers over SSH, multi-server fan-out, jump hosts, and cloud deployment.
- Kubernetes and Swarm orchestration.
- Coolify feature parity: wildcard domains, ACME and managed DNS, backups as a service.
- VM-grade (Hyper-V) isolation on Windows; revisit as a fresh effort if a concrete requirement appears.
- Linux desktop packaging and substrate support in this effort.
- Windows machines where WSL2 cannot be enabled (the app refuses with an explanation rather than degrading silently).
