# Spec: Agentify v0 — agent-native local environments

Status: ready-for-agent
Source map: `../agent-native-environments/map.md`

## Problem Statement

An AI agent can write a whole feature but cannot own the environment it runs in. Setting up a project locally still means installing language runtimes, databases, and Docker by hand; every project competes for the same ports, global packages, and credentials; and two agents working at once can trample each other. Existing local tools are manual, and server tools such as Coolify assume SSH, multi-tenancy, and Docker on a remote host.

The result is that agents stop at "run these commands", humans spend their time repairing environments, and nothing enforces that one project cannot see another's data.

## Solution

Agentify gives every Project its own isolated runtime on the developer's machine, with one control plane shared by agents and humans. A Project is a Manifest plus an Environment. The daemon provisions a per-Project runtime — a WSL distro on Windows, an Apple container microVM or Lima VM on macOS — starts its Resources, routes them at `<resource>.<project>.localhost`, and exposes everything through a loopback API.

Agents use an MCP server with twenty verb-first tools. Humans use a desktop UI. Destructive operations require an OS-authenticated Approval. Nothing reaches the network beyond loopback.

v0 proves one vertical slice: a Node 22 app and Postgres 16, created and driven end to end by an Agent, observed and approved by a human, on Windows 11.

## User Stories

### Projects and Environments

1. As an Agent, I want to create a Project from a bundled fixture, so that I can start working without a manual scaffold.
2. As an Agent, I want to create a Project from an inline Manifest, so that I can define an app and a database without touching the host.
3. As a developer, I want every Project to have its own runtime, so that two Projects cannot share toolchains, ports, or data.
4. As an Agent, I want `list_projects` to return state and resource cost, so that I can see what is running.
5. As a developer, I want to inspect a Project's Environment and Resources, so that I know what is healthy.
6. As a human, I want deleting a Project to require my Approval, so that an Agent cannot destroy work silently.
7. As an Agent, I want a typed `AlreadyExists` error on a duplicate Project name, so that retries are unambiguous.
8. As an Agent, I want `whoami` to return my Project, scopes, versions, and capabilities, so that I do not discover limits through failures.
9. As an Agent, I want `start_environment` to be idempotent and convergent, so that retrying after a timeout is safe.
10. As an Agent, I want `start_environment` to return when every Resource is running, so that my next command is deterministic.
11. As a developer, I want `stop_environment` to keep the runtime and data, so that resuming is fast.
12. As a developer, I want `status` to report state, health, Resources, ports, and cost, so that the UI and Agents share one truth.
13. As an Agent, I want the daemon to decide health through leased host ports, so that every Backend behaves the same.
14. As a developer, I want a stopped Environment to keep its URL and return a clear 503, so that links do not rot.
15. As a developer, I want a port conflict on 80 or 443 to fail loudly and name the holder, so that I can fix the machine.

### Resources, exec, and observation

16. As an Agent, I want to run a bounded command in a Resource and receive stdout, stderr, and the exit code, so that I can migrate, test, and inspect.
17. As an Agent, I want `exec` to be cancellable and time-bounded, so that a hung command cannot wedge me.
18. As an Agent, I want `exec` to inherit the Resource's environment, so that migrations can authenticate.
19. As a developer, I want live logs per Resource, so that I can debug without SSH.
20. As an Agent, I want a bounded log tail with `since` and `tail`, so that I do not flood my context.
21. As a developer, I want per-Resource and per-Project CPU, memory, and disk figures, so that I can see cost.

### Manifest

22. As a developer, I want a strict Manifest that rejects unknown fields, so that a typo fails loudly instead of silently doing nothing.
23. As a developer, I want to declare an app by image or by build, so that I can use either.
24. As a developer, I want to declare one or more ports with hostnames, so that each service gets a stable URL.
25. As a developer, I want dependency ordering between Resources, so that the app starts after the database.
26. As a developer, I want a declared healthcheck with a TCP default, so that readiness is explicit.
27. As a developer, I want secrets declared by name and referenced, never inlined, so that values stay out of the file.
28. As a developer, I want database volumes named or generated, so that I do not invent names.
29. As a developer, I want the Manifest to reject privileged mode, host networking, host PID, and Docker socket mounts, so that a file cannot break isolation.

### Ingress and TLS

30. As a developer, I want `https://web.<project>.localhost` with a locally trusted certificate, so that cookies, service workers, and secure APIs work.
31. As a developer, I want HTTP to keep working when I refuse the CA root, so that the Project is still usable.
32. As a developer, I want WebSockets and SSE to pass through, so that real applications work.
33. As a developer, I want each Resource port routed to its own hostname, so that multiple services are addressable.
34. As a developer, I want ingress bound to loopback only, so that my Projects are not exposed on the LAN.
35. As a developer, I want uninstall to remove the trusted root, so that no stale trust remains.

### Secrets

36. As a developer, I want generated database passwords stored in the OS keyring, so that no plaintext lives on disk.
37. As a developer, I want to import a secret from an environment variable once, so that a Project is reproducible.
38. As an Agent, I want to create and update secrets and reference them, so that I can wire a Project.
39. As an Agent, I want the API to never return plaintext, so that the API is not an exfiltration path.
40. As a human, I want to reveal a secret after an OS authentication prompt, so that I can use an external database client.
41. As a security reviewer, I want every secret reveal audited without the value, so that I can reconstruct access.
42. As a security reviewer, I want known secret values redacted from logs and exec output, so that they do not leak casually.

### Data

43. As a developer, I want data to survive daemon restarts and stop/start, so that work is not lost.
44. As an Agent, I want to create a Checkpoint before a risky change, so that I can restore.
45. As an Agent, I want restore to require Approval, so that I cannot overwrite data silently.
46. As an Agent, I want reset to drop and recreate databases and restart dependents, so that migrations re-run.
47. As a developer, I want deleting a Project to remove its runtime, volumes, routes, leases, and Checkpoints, so that nothing is left behind.
48. As a developer, I want teardown verified and reported, so that I can trust "deleted".
49. As a developer, I want orphan runtimes reported rather than auto-deleted, so that a lost database row cannot destroy work.

### Agent API and MCP

50. As an Agent, I want twenty verb-first MCP tools, so that I can act without shelling out.
51. As an Agent, I want a long operation to return an operation id on timeout, so that I can wait instead of retrying blindly.
52. As an Agent, I want an Approval-requiring call to return 202 with ids, so that a slow human does not break my flow.
53. As an Agent, I want stable problem codes, so that I can branch on failures instead of parsing prose.
54. As an Agent, I want idempotency keys, so that retries do not duplicate work.
55. As an Agent, I want coarse `read`, `operate`, and `admin` scopes, so that I know what I may do.
56. As a developer, I want one API for UI, CLI, and MCP, so that no privileged path exists.
57. As an Agent, I want the MCP server over stdio, so that existing clients can connect.
58. As a security reviewer, I want Agent tokens to expire and be revocable, so that a leaked token has a bounded life.

### Approvals and audit

59. As a human, I want Approvals to be single-use, parameter-bound, and expiring, so that a yes cannot be replayed or repurposed.
60. As a human, I want to approve with Windows Hello or Touch ID, so that an Agent cannot approve its own request.
61. As a human, I want the Approval card to show operation, target, requester, parameters, and expiry, so that I can judge it.
62. As a human, I want Agent and human actions in one activity feed, so that I can see what happened.
63. As a security reviewer, I want a hash-chained audit, so that tampering is detectable.
64. As a developer, I want audit entries to exclude values, so that the log is safe to share.

### Desktop UI

65. As a human, I want a Project rail with state and per-Project cost, so that I can see the machine at a glance.
66. As a human, I want a detail pane with Resources, health, URL, and logs, so that I can diagnose.
67. As a human, I want Start, Stop, Approve, and Deny in the UI, so that I do not need the CLI.
68. As a human, I want empty states that explain the next action, so that first run is not a blank screen.

### Isolation and security

69. As a developer, I want Project A to be unable to read B's volumes, authenticate to B's database, or resolve B's hostname, so that Projects are isolated.
70. As a developer, I want separate volumes and database credentials per Project, so that data isolation does not rely on routing.
71. As a developer, I want no host mounts in v0, so that a Manifest cannot read my home directory.
72. As a developer, I want outbound network allowed and inbound loopback-only, so that installs work without exposing services.
73. As a developer, I want caps on Projects and concurrent starts, so that an Agent loop cannot exhaust the machine.
74. As a developer, I want a plain statement that Windows shares a kernel and network namespace, so that I know what isolation means.

### Distribution and operations

75. As a developer, I want a signed per-user installer with no admin, so that installation is frictionless.
76. As a developer, I want first run to detect WSL and offer one UAC step with a reboot resume, so that setup is not a scavenger hunt.
77. As a developer, I want the daemon to start at login and restart on crash, so that Agents can connect without the UI.
78. As a developer, I want signed auto-updates that never interrupt an operation and can roll back, so that updates are safe.
79. As a developer, I want running runtimes adopted across an update, so that an update does not kill my work.
80. As a developer, I want uninstall to leave my Projects unless I confirm removal, so that uninstalling is not destructive.
81. As a developer, I want pinned runtime versions and refused mismatches, so that Backends do not drift silently.
82. As a macOS developer, I want Apple container on macOS 26+ and Lima on 14–15, so that I get VM isolation where possible.
83. As a macOS developer, I want one user trust prompt and no admin, so that setup stays clean.
84. As an Intel Mac user, I want a clear refusal rather than a broken install.

## Implementation Decisions

### Stack and modules

One Rust workspace: a long-lived daemon, a CLI, and an MCP stdio shim. The desktop UI is a Tauri shell rendering in the OS webview. State lives in embedded SQLite in WAL mode. Caddy is a bundled sidecar. Secrets use the OS credential manager. YAML is deserialized strictly into a closed schema. This follows ADR-0005 and ADR-0003.

Modules, described by responsibility rather than path:

- **daemon** — owns state and all mutations; HTTP API, operation lifecycle, Approvals, audit, port leases, health, reconciliation, ingress control.
- **manifest** — pure parse and normalize step producing an EnvironmentSpec; no I/O.
- **backends** — the Backend contract plus WSL, Apple container, and Lima adapters, and a scripted fake for tests.
- **ingress** — Caddy admin client, route model, trust installation and removal.
- **secrets** — keyring-backed store, generation, import, redaction.
- **data** — Checkpoint archives, reset, teardown and verification.
- **mcp** — stdio shim mapping the twenty tools onto the API.
- **cli** — token issue and revoke, secret reveal, diagnostics.
- **ui** — the Tauri shell and its views; no business logic of its own.

### Domain model

`Project → Environment → Resource`. A Project is a named workspace bound to one host directory (or, in v0, a bundled fixture) and owns Environments. v0 recognises only `dev`. A Resource is an app, database, or service inside an Environment.

`Backend` replaces Coolify's `Destination`: it is the runtime handle for an Environment. There is no `Team` and no `Server`; this is a single-user, single-machine product (ADR-0004).

### Backend contract

The contract is environment-scoped and lifecycle-only. It hides whether an Environment is one distro with several containers, a set of per-container microVMs, or a VM.

| Operation | Signature | Notes |
|---|---|---|
| `describe` | `() → BackendInfo` | name, version, platform, capabilities |
| `start` | `(EnvironmentSpec) → ()` | idempotent and convergent |
| `stop` | `(EnvId) → ()` | stops workloads, keeps runtime and volumes |
| `remove` | `(EnvId, keepVolumes) → ()` | destroys runtime; Approval-gated |
| `status` | `(EnvId) → Status` | snapshot |
| `events` | `(EnvId) → Stream<Event>` | optional capability |
| `exec` | `(EnvId, target?, ExecSpec) → ExecSession` | streamed, cancellable, TTY if capable |
| `logs` | `(EnvId, target?, LogOptions) → Stream<LogLine>` | follow, since, tail |
| `stats` | `(EnvId) → Stats` | environment-level required, per-Resource optional |
| `copyIn` / `copyOut` | `(EnvId, target?, hostPath, guestPath) → ()` | host paths are daemon-owned |
| `checkpoint` / `restore` / `deleteCheckpoint` | `(EnvId[, CheckpointId]) → CheckpointId` | optional capability |

Rules: lifecycle calls are idempotent; `start` re-creates missing workloads; the Backend owns restart policy inside the runtime; the daemon serialises mutations per Environment; long calls accept cancellation; port leases are allocated by the daemon, released on `stop`, reacquired on `start`; host paths are daemon-owned; secrets are resolved by the daemon and redacted by the Backend.

Readiness: `start` returns when every Resource is running. The daemon probes health through leased host ports and owns the `environment.up` decision.

Status states: `absent`, `provisioning`, `stopped`, `starting`, `running`, `degraded`, `failed`. Backend events: `state_changed`, `resource_exited`, `lease_lost`, `backend_unavailable`. Health is a daemon-level event.

Errors: `Unsupported`, `VersionMismatch`, `NotProvisioned`, `AlreadyExists`, `PortUnavailable`, `ExecutionFailed`, `BackendUnavailable`, `Timeout`, `Cancelled`. Capabilities are `pty`, `mount`, `checkpoint`, `events`, `resourceStats`, `logSince`.

### Manifest v0

From the manifest prototype, the decision-rich shape is:

```yaml
version: 1
project: hello-pg

secrets:
  pg-password: { generate: password }
  api-token: { fromEnv: HELLO_API_TOKEN }

resources:
  web:
    kind: app
    build: { context: ., dockerfile: Dockerfile }
    command: [node, server.js]
    env:
      DATABASE_URL: postgres://hello:${secret:pg-password}@db:5432/hello
      API_TOKEN: ${secret:api-token}
      GREETING: hello
    ports:
      - { port: 3000, hostname: web }
    dependsOn: [db]
    healthcheck: { port: 3000, path: /health, interval: 5s }

  db:
    kind: database
    engine: postgres
    version: "16"
    credentials:
      username: hello
      password: ${secret:pg-password}
```

Rules: `version`, `project`, and `resources` are required; resources are a map; an app has exactly one of `image` or `build`; a single port defaults its hostname to the Resource name, while more than one requires every hostname; routes are unique per Project; unknown fields are rejected at every level; secrets are named sources (`generate: password|token` or `fromEnv: NAME`) referenced as `${secret:name}`; volume names are generated as `<project>-<resource>-data` unless pinned; `healthcheck` defaults to a TCP probe on the first port.

No `service` kind, no seed field, no build-time secrets, no environment overrides in v0.

### Isolation and runtime

- Windows: one WSL2 distro per Project, created with `wsl --import` after a one-time elevated WSL enable. The boundary is filesystem, process, and cgroup isolation over a shared kernel and network namespace. Data lives on ext4 inside the distro, never `/mnt/c`.
- macOS 26+/Apple silicon: Apple `container`, one microVM per container, driven through its CLI JSON surface.
- macOS 14–15/Apple silicon: Lima with `vmType: vz`, one VM per Project. Intel is unsupported.
- The daemon owns a machine-wide port registry. Bindings are `127.0.0.1:<lease>`; a bound guest port is reachable from the host at `localhost:<lease>` (verified on Windows).
- Each Project runs under a foreground supervisor; background processes launched by a one-shot launcher do not survive on Windows. There is no PTY in v0; interactive terminals are deferred.
- Volume data is Environment-owned and backend-specific; the daemon passes volume names, never paths.

### Ingress and TLS

Routes are `<resource>.<project>.localhost`, HTTP(S) only, loopback-only, served by one Caddy instance the daemon controls through its admin API on a random loopback port. Each route has an `@id`; updates are granular, with a full load at startup and recovery.

HTTPS is primary; HTTP redirects when the CA root is trusted and serves directly when the user refused it. A stopped Environment keeps its route and returns a daemon-authored 503; only removing the Environment deletes the route. Unhealthy apps pass through untouched. Routes set `X-Forwarded-For`, `X-Forwarded-Proto`, and `X-Forwarded-Host`; response buffering is disabled for SSE; WebSockets use Caddy's defaults; no artificial timeouts.

Port 80 or 443 unavailable fails with `PortUnavailable` and names the holder. No silent port change, no custom domains, no LAN exposure, no raw TCP routes.

Trust: Windows writes the per-user root with no elevation and enables Firefox enterprise roots; macOS sets `install_trust false` and uses a user-domain `security add-trusted-cert` with one prompt; uninstall removes the root and Caddy data.

### Secrets

Project-scoped, stored in the OS keyring, injected as runtime environment variables only. `fromEnv` is imported once at Project creation (with `HKCU\Environment` as the Windows fallback) and then immutable. The API never returns plaintext; Agents may create and update secrets and delete unreferenced ones; reading plaintext is human-only through an OS-authenticated reveal. Redaction removes exact values and their percent-encoded forms from logs, exec output, status, errors, and audit.

The enforceable boundary is Project-to-Project isolation plus audit. An Agent executing code inside a Project can read what that Resource can read.

### Data

Volumes are daemon-named and Environment-owned; v0 has database volumes only. `remove` is destructive; `keepVolumes` is `Unsupported` until a later effort, and `stop` is the preservation path.

Checkpoints are host-side database-native archives produced by `pg_dump` in the runtime and copied out, backend-neutral. One Checkpoint covers every database Resource in the Environment as a set, with best-effort cross-engine consistency. Creation and deletion are explicit; there are no automatic Checkpoints; sizes appear in stats.

`reset` drops and recreates each database and restarts dependents so entrypoint migrations re-run. `restore` stops dependents, restores, and restarts them. Both require Approval. `project.delete` removes runtime, volumes, routes, leases, and Checkpoints together, with teardown verification. Orphan runtimes, volumes, routes, and leases are reported at startup and never auto-deleted.

### API

REST over loopback HTTP/1.1 under `/v1`, with SSE for events and log following and WebSocket for exec. The MCP server is a stdio shim over the same API.

Endpoints: Projects (`GET/POST /v1/projects`, `GET/DELETE /v1/projects/{project}`); Environment (`GET .../environments`, `POST .../{env}/start|stop|reset`, `GET .../{env}/status`); Resources (`GET .../resources`, `POST .../resources/{resource}/exec`, `GET .../logs`, `GET .../stats`); Checkpoints (`GET/POST .../checkpoints`, `POST .../{id}/restore`, `DELETE .../{id}`); Secrets (`GET/POST .../secrets`, `PATCH/DELETE .../{name}`); Operations (`GET /v1/operations/{id}`, `GET .../wait`); Events (`GET /v1/events`, SSE); Exec (`GET /v1/exec/{id}`, WebSocket); System (`GET /v1/whoami`).

Mutating calls block until complete or timeout; a timeout returns `504` with the `operation_id`. An Approval-requiring call returns `202` with `approval_id` and `operation_id`; the Agent waits on the operation.

Scopes are per-Project and coarse: `read`, `operate`, `admin`. Agent tokens expire after seven days by default and are revocable. The discovery file holds an Agent-scoped token, never a human-capable one, with a user-only ACL.

Approvals are single-use, bound to a hash of the operation and parameters, expire after fifteen minutes, and require an interactive OS gesture in the UI (Windows Hello, macOS LocalAuthentication) producing a short-lived assertion bound to the Approval id. They gate `reset_environment`, `restore_checkpoint`, and `delete_project`.

Errors are `application/problem+json` with a stable `code`: the Backend enum plus `NotFound`, `ValidationFailed`, `SecretSourceMissing`, `SourceMissing`, `ApprovalRequired`, `ApprovalExpired`. `Idempotency-Key` is supported on every mutation and replays the original response for 24 hours.

Audit entries are append-only and hash-chained: `{ id, at, actor: { kind, id, token_id }, project_id, operation, target, params_hash, approval_id?, result, error_code?, duration_ms, request_id, prev_hash, hash }`, with values excluded. Reads verify the chain; it is detectable, not tamper-proof.

### MCP tools

`list_projects`, `get_project`, `create_project`, `delete_project`, `start_environment`, `stop_environment`, `get_status`, `reset_environment`, `run_command`, `read_logs`, `get_stats`, `create_checkpoint`, `list_checkpoints`, `restore_checkpoint`, `list_secrets`, `create_secret`, `update_secret`, `delete_secret`, `wait_for_operation`, `whoami`.

`create_project` takes exactly one of a bundled fixture or an inline Manifest; a Manifest using `build` without a fixture fails with `SourceMissing`. `run_command` returns stdout, stderr, and exit code with no interactive stdin. `read_logs` returns a bounded tail by default.

### Desktop UI

B's header and light visual language over A's three-zone shell. Header: brand plus `N Projects · daemon connected · api v1`. Left rail: Projects with state dot, name, and `cpu · memory`. Main: Project title with Environment badge, URL, status bar, Start and Stop, `Overview` and `Logs` tabs, Resource table. Right: Approval card above the activity feed.

Shipped: Project rail, Environment detail, live logs, Approval inbox, activity feed. Not shipped: Manifest editor, interactive terminal, secret-management UI, separate audit screen. Selecting a Project, Start, Stop, Approve, Deny, log following, and create all map to API calls; the UI has no privileged path.

### Distribution and bootstrap

Windows: per-user NSIS installer, Authenticode-signed, no admin, shipping the app, daemon sidecar, Caddy, and helpers. First run detects WSL without elevation (`wsl --version`, optional-feature state, `HypervisorPresent`; never gate on SLAT), offers an explicit UAC `wsl --install --no-distribution` with a RunOnce resume, downloads the pinned Ubuntu base rootfs, starts the daemon and loopback Caddy, and installs the per-user root with Firefox enterprise roots.

macOS: notarized DMG, Developer ID, hardened runtime, Apple silicon only. Apple container on 26+, Lima on 14–15. `SMAppService` login item. One user-domain trust prompt. The `com.apple.vm.networking` entitlement is filed early; loopback publishing is used until granted.

Linux: excluded from v0.

One non-roaming data root (`%LOCALAPPDATA%\Agentify\` or `~/Library/Application Support/Agentify/`) holds the state database, logs, Checkpoints, rootfs cache, runtime disks, and the discovery file.

Daemon lifecycle: per-user login item with auto-restart; never a Windows service.

Version pinning: API v1, daemon and MCP shipped together, Caddy version, Backend adapter majors, and rootfs and image digests. A mismatch yields `VersionMismatch` and refuses that Backend.

Updates: signed, stable by default with an opt-in beta; background download; applied on quit or restart; refused while an operation is in flight. One previous version is kept; the new daemon must pass health and adopt existing runtimes or the app rolls back. Running runtimes are never stopped by an update; failed adoption marks the Project degraded. Substrate images update separately behind a compatibility check.

Telemetry: opt-in crash reports only, local logs with bounded retention, no analytics.

Uninstall: removes the app, sidecar, login item, Caddy data, and trusted root; leaves Projects. Removing runtimes, volumes, Checkpoints, and the state database is a separate confirmed action.

### Persistence

SQLite tables: `projects`, `environments`, `resources`, `port_leases`, `operations`, `approvals`, `secrets_metadata`, `checkpoints`, `audit`, `agent_tokens`, and `schema_migrations`. No secret values, no file paths to arbitrary host locations, and no plaintext tokens — token hashes only.

### Security boundary

Same-user processes are inside the trust boundary: they can call the daemon API, read the discovery file, and reconfigure Caddy through its admin port. On Windows, a root inside one distro can reach another Project's loopback listeners because the kernel and network namespace are shared. No kernel isolation is claimed on Windows.

Enforced: Project-scoped tokens, OS-authenticated Approvals, no host mounts, separate volumes and database credentials, loopback-only ingress, and a hash-chained audit. Manifests cannot request privileged mode, host networking, host PID, or a Docker socket. Images may use tags; digests are supported; signature verification is an accepted gap for v0. Abuse caps: 20 Projects and two concurrent starts, with a disk-usage warning.

### Testing seams

Two new seams, agreed before writing:

1. **Public API** — black-box acceptance tests drive the daemon's `/v1` over HTTP, SSE, WebSocket, and the MCP shim. This is the primary seam; every user story is exercised here, and no test asserts on internal modules.
2. **Backend contract** — a shared conformance suite runs against a scripted fake Backend in CI and against the real Windows and macOS adapters on their machines. It is the substitution point that makes the API suite fast and the substrates interchangeable.

Manifest validation, MCP tools, and ingress are exercised through the API seam; they do not get their own seams. The UI is covered through the API plus a small browser-level check of the Approval interruption.

## Testing Decisions

A good test asserts external behavior: an HTTP request and its response, an SSE event, a WebSocket frame, an audit entry, or an observable OS effect (the URL serving, the volume gone, the distro unregistered). Tests do not reach into Rust modules, assert on private state, or mock anything except the Backend fake.

- **Acceptance suite (API seam).** Runs the real daemon with the fake Backend for determinism and with the real WSL Backend for the slice. It mirrors the ten acceptance checks below. It asserts status codes, problem codes, operation ids, Approvals, audit entries, and teardown.
- **Backend conformance suite.** One table of cases every Backend must pass: lifecycle idempotence, convergence after deletion, exec streaming and exit codes, cancellation, log tailing, port binding, capability reporting, typed errors. The fake Backend runs in CI on every OS; the WSL adapter runs on Windows; Apple container and Lima adapters run on macOS.
- **Manifest tests.** Table-driven, through the API seam: valid minimal, the vertical slice, multi-port with and without hostnames, unknown fields, undefined secret references, duplicate routes, dependency cycles, forbidden privilege fields.
- **Security tests.** Cross-Project probes (A cannot read B's volume, authenticate to B's database, or resolve B's hostname), Approval bypass attempts with an Agent token, expired and replayed Approvals, path traversal through copy operations, redaction of known values in logs and exec output, token scope enforcement, and audit chain verification.
- **UI check.** A browser-level test for the one interaction with real risk: an Agent-initiated destructive request appears as an Approval, and Approve/Discard changes state only after the OS-auth path returns.
- **Prior art.** None; the repository is greenfield. These suites establish the pattern: API-level acceptance, contract conformance per adapter, and no test-only production seams.

Acceptance checks for the slice:

1. A scripted MCP client calls `project.create` with the fixture; the Project appears with one `dev` Environment and two Resources.
2. `environment.up` returns after health checks pass, and the app is healthy.
3. `https://web.hello.localhost` returns 200 over a trusted certificate and renders a row from Postgres.
4. `exec` runs the fixture migration and the row appears.
5. Restarting the daemon leaves the row intact and the URL serving.
6. `environment.reset` blocks on an Approval; after approval the table is empty.
7. `checkpoint.create` → mutate → `checkpoint.restore` (approved) brings the row back.
8. With a second Project up, A cannot read B's volumes, authenticate to B's database, or resolve B's hostname.
9. `project.delete` blocks on an Approval; after approval the distro, volumes, Caddy route, and port lease are gone, and `wsl -l -v` shows no leftover.
10. Every mutation appears in the audit log with actor, token, operation, and target.

## Out of Scope

- Multi-tenant teams, users, roles, invitations, and billing.
- Remote servers over SSH, multi-server fan-out, jump hosts, and cloud deployment.
- Kubernetes and Swarm orchestration.
- Coolify feature parity: wildcard domains, ACME and managed DNS, backups as a service.
- VM-grade (Hyper-V) isolation on Windows.
- Linux desktop packaging and substrate support in this effort.
- Windows machines where WSL2 cannot be enabled; the app refuses with an explanation.
- Custom domains, LAN exposure, and raw TCP routes.
- Host mounts, source editing/sync, and file upload/download tools; v0 creates from a bundled fixture or an inline Manifest.
- App-level persistent volumes, `keepVolumes`, and Project export/import.
- Build-time secrets, private registry authentication, secret rotation, and image signature verification.
- Checkpoint encryption at rest.
- Multi-Environment semantics, template catalogs, per-Project CPU/memory budgets, and concurrent collaboration.
- Manifest editor, interactive terminal, and secret-management UI.
- Analytics and any telemetry beyond opt-in crash reports.

## Further Notes

- Windows isolation is namespace-level, not kernel-level. The spec, UI copy, and docs must say so plainly rather than implying parity with macOS.
- The manifest and UI prototypes are kept as primary sources: the manifest prototype argues the schema through failure cases, and the UI prototype holds three layouts, with the chosen hybrid described above rather than rendered as a fourth.
- Research notes that shape the design: per-project WSL provisioning (including the shared-netns measurement), macOS container backends, and local CA trust without elevation.
- The five ADRs record the hard-to-reverse choices: per-project isolation, Manifest as source of truth, the daemon as the only control plane, dropping Team and Server, and the Rust/Tauri stack.
- The v0 slice is deliberately narrow: one app, one database, Windows, agent-first, with a human observing and approving. Breadth — more languages, more services, macOS, multiple Environments — comes after the slice passes all ten checks.
- macOS Backends and macOS distribution are deferred to a follow-on effort after the Windows slice; their user stories remain in this spec as the target for that effort.
