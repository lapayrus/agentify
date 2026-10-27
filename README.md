# Agentify

A local, agent-native environment manager. Each Project runs in its own isolated
runtime on the developer's machine, and one control plane exposes it to agents
and humans alike. Read `GLOSSARY.md` for the domain language, `docs/adr/` for the
hard-to-reverse decisions, and `.scratch/agentify-v0/spec.md` for the product spec.

**Current status: ticket 03, Environment lifecycle on the fake Backend.** There
is a daemon with state, a loopback API, an MCP server, a CLI, and a desktop
shell. Projects can be created from the bundled `hello-pg` fixture or an inline
Manifest, and their `dev` Environment can be started, stopped, inspected, and
driven: port leases, health, exec, logs, and stats all run end to end against a
scripted fake Backend. Nothing touches a real substrate yet — no WSL — but local
ingress (`*.localhost` through Caddy), Approvals with an audit trail, and the
Project secret store are wired and tested at the API seam.

## Layout

| Path | What it is |
| --- | --- |
| `crates/proto` | Wire types, discovery-file format, data-root resolution, and the API client shared by every process |
| `crates/manifest` | Pure Manifest parse and normalization into a spec: strict, path-accurate errors, no I/O |
| `crates/backends` | The Backend contract (capabilities, typed errors, an object-safe async trait), the scripted fake, and the conformance suite every adapter runs |
| `crates/ingress` | Local ingress: the route model, the Caddy admin client, Caddy lifecycle, port probing, and the trust installer seam |
| `crates/daemon` | `agentifyd`: owns state (SQLite), port leases, health, operations, and the `/v1` API; embeds the bundled fixtures |
| `crates/mcp` | `agentify-mcp`: MCP stdio shim over the same API |
| `crates/cli` | `agentify`: `daemon`, `status`, `whoami` |
| `fixtures/` | Bundled fixtures compiled into the daemon (`hello-pg`) |
| `ui/` | Desktop front end: plain HTML/CSS/JS modules, built into `ui/dist` |
| `ui/src-tauri` | Tauri v2 shell crate (its own workspace; embeds `ui/dist`) |

The daemon is the only control plane (ADR-0003). The shell, the CLI, and the MCP
shim are clients of the same API; none of them has a privileged path.

## Backends

A **Backend** is the runtime substrate that hosts an Environment. v0 ships one:
`fake`, a scripted implementation selected with `AGENTIFY_BACKEND=fake` (the
default). It binds a TCP listener on every leased host port so the daemon's own
health probe succeeds or fails for a real reason, moves
`absent → starting → running → stopped`, and can be scripted to fail or degrade:

```sh
AGENTIFY_FAKE_SCRIPT='{"start_delay_ms": 250, "unbound_resources": ["web"]}' agentifyd
```

The daemon — never the Backend — allocates ports and decides health. The WSL
adapter arrives in a later ticket; `crates/backends` holds the contract and a
[conformance suite](crates/backends/src/conformance.rs) that every adapter must
pass before the API has to learn about it.

### Daemon knobs

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGENTIFY_HOME` | OS data dir | Data root |
| `AGENTIFY_BACKEND` | `fake` | Which Backend to drive |
| `AGENTIFY_FAKE_SCRIPT` | — | JSON script for the fake |
| `AGENTIFY_OPERATION_TIMEOUT_MS` | `30000` | How long a mutating call blocks before answering `504` with an operation id |
| `AGENTIFY_LEASE_RANGE` | `40000-40999` | Host ports the daemon may lease (`40000-40099`, or a single `40000`) |
| `AGENTIFY_PROBE_TIMEOUT_MS` | `750` | How long one TCP health probe waits |
| `AGENTIFY_EXEC_TIMEOUT_MS` | `60000` | Upper bound on one `exec` |
| `AGENTIFY_LOG_TAIL` | `200` | Default log tail |
| `AGENTIFY_HEARTBEAT_MS` | `15000` | SSE heartbeat period |
| `AGENTIFY_SECRET_STORE` | `keyring` | Where secret values live: `keyring` (the OS credential manager) or `memory` (a test-only store) |
| `AGENTIFY_SECRET_SERVICE` | `agentify-v0` | Keyring service name the values are filed under |
| `AGENTIFY_INGRESS` | on | Set to `off` to run without local ingress (no Caddy, no `*.localhost` routes) |
| `AGENTIFY_CADDY` | search | Path to the `caddy` executable; none is downloaded |
| `AGENTIFY_HTTP_PORT` | `80` | Plain-HTTP ingress port |
| `AGENTIFY_HTTPS_PORT` | `443` | TLS ingress port |
| `AGENTIFY_CADDY_ADMIN` | spawn | Attach to an already-running Caddy admin endpoint (`127.0.0.1:2019`) instead of spawning one |
| `AGENTIFY_TRUST` | — | Explicit trust action at startup: `install` or `remove`; nothing else touches the certificate store |

## Run it

```sh
cargo run -p agentify-daemon --bin agentifyd      # foreground daemon, Ctrl-C to stop
cargo run -p agentify-cli -- status              # daemon connected at http://127.0.0.1:<port>
cargo run -p agentify-cli -- whoami              # the whoami payload as JSON
```

Point any process at a scratch data root with `AGENTIFY_HOME`:

```sh
AGENTIFY_HOME=./scratch cargo run -p agentify-daemon --bin agentifyd
```

### MCP

Register the shim with an MCP client (stdio transport):

```json
{
  "mcpServers": {
    "agentify": {
      "command": "<path to target/debug/agentify-mcp>",
      "env": { "AGENTIFY_HOME": "<optional data root>" }
    }
  }
}
```

`initialize`, `tools/list`, and `tools/call` work today. Every tool result is
byte-for-byte the payload of the matching HTTP route:

```json
{"name": "create_project", "arguments": {"fixture": "hello-pg"}}
{"name": "list_projects", "arguments": {}}
{"name": "get_project", "arguments": {"project": "hello-pg"}}
{"name": "start_environment", "arguments": {"project": "hello-pg"}}
{"name": "get_status", "arguments": {"project": "hello-pg"}}
{"name": "run_command", "arguments": {"project": "hello-pg", "resource": "web", "command": ["node", "--version"]}}
{"name": "read_logs", "arguments": {"project": "hello-pg", "resource": "web", "tail": 50}}
{"name": "get_stats", "arguments": {"project": "hello-pg"}}
{"name": "list_secrets", "arguments": {"project": "hello-pg"}}
{"name": "create_secret", "arguments": {"project": "hello-pg", "name": "session-key", "generate": "token"}}
{"name": "update_secret", "arguments": {"project": "hello-pg", "name": "session-key", "value": "<new value>"}}
{"name": "delete_secret", "arguments": {"project": "hello-pg", "name": "session-key"}}
{"name": "wait_for_operation", "arguments": {"operationId": "<id from a timed-out mutation>"}}
{"name": "stop_environment", "arguments": {"project": "hello-pg"}}
```

Tools: `whoami`, `create_project`, `list_projects`, `get_project`,
`start_environment`, `stop_environment`, `get_status`, `run_command`,
`read_logs`, `get_stats`, `list_secrets`, `create_secret`, `update_secret`,
`delete_secret`, `wait_for_operation`. `environment` defaults to `dev`.

Secrets are metadata over MCP: a list carries names and provenance, never a
value, and `delete_secret` refuses (`SecretInUse`) while the Manifest still
references the name. Reading a value is human-only, through the OS gesture, and
has no MCP tool.

A tool failure is a JSON-RPC error whose `data` carries the same stable `code`
the API returns (`ValidationFailed`, `AlreadyExists`, `SourceMissing`,
`NotFound`, `PortUnavailable`, `NotProvisioned`, `BackendUnavailable`, `Timeout`,
`Unsupported`, …), plus `path` for a validation failure and `operationId` when a
mutation timed out.

### Desktop shell

The shell is a Tauri v2 window and must be built separately because it embeds
webview assets:

```sh
cd ui
npm ci && npm run build          # static assets into ui/dist
npm test                         # renderer unit tests
cd src-tauri
cargo check --all-targets        # shell crate
cargo run                        # opens the window (needs a daemon for "connected")
```

## Install (Windows, per user)

The shell is also an installer. It is a per-user NSIS bundle: no administrator
rights, no UAC prompt, and everything it writes stays inside the current user's
profile.

```sh
cd ui
npm ci
npm run bundle                   # tauri build --bundles nsis
# -> ui/src-tauri/target/release/bundle/nsis/Agentify_<version>_x64-setup.exe
cd src-tauri && cargo test       # the shell's own suite
```

`npm run bundle` runs `stage-sidecar` first, which builds and stages the three
binaries the installer ships beside the app: `agentifyd.exe` (the daemon), the
`agentify-mcp.exe` MCP shim, and the `agentify.exe` CLI. Tauri strips the target
triple and installs them next to `agentify-shell.exe`, which is where
`login_item::daemon_binary()` looks.

| What | Where |
| --- | --- |
| Program, sidecars, uninstaller | `%LOCALAPPDATA%\Programs\Agentify\` |
| Data root: `state.db`, `discovery.json`, `logs\`, `checkpoints\`, `projects\`, `wsl\`, `ingress\`, `tools\` | `%LOCALAPPDATA%\Agentify\` |

The two are siblings on purpose. Tauri's per-user default is
`%LOCALAPPDATA%\<productName>`, which is exactly the data root, so
`ui/src-tauri/installer-hooks.nsh` moves the default to `%LOCALAPPDATA%\Programs`
(a directory the user picks is left alone). Installing over a version replaces
the program directory only, so Projects, Checkpoints, the rootfs cache, and the
state database survive.

The same hook file registers the daemon's login item: `POSTINSTALL` runs
`agentify login-item install`, `PREUNINSTALL` runs `agentify login-item remove`.
That is a per-user Scheduled Task (`Agentify Daemon`) with a logon trigger, an
interactive token at `LeastPrivilege`, a `PT0S` execution limit, and restart on
failure — never a Windows service, so the daemon runs in the user's own session
and `%LOCALAPPDATA%` and keeps running with the UI closed. `agentify login-item
status` reports what the scheduler holds.

The bundle is signing-ready but unsigned locally: `bundle.windows` already names
`digestAlgorithm: sha256` and a timestamp URL, and `certificateThumbprint` is
`null`. Set it to a certificate thumbprint in the machine's store (or switch to
`signCommand`) and the app and installer are Authenticode-signed with no other
change. Updater artifacts are off; turning them on also needs
`TAURI_SIGNING_PRIVATE_KEY`.

First run is the one place Agentify asks for an administrator prompt.
`agentify bootstrap` only reports: exit 0 and `ready` on a machine that already
has WSL 2, exit 2 and `consent_required` when one elevated step is still needed,
and nothing is installed either way. Consenting is a human action — the setup
card in the shell, or `agentify bootstrap --consent` — and both run the same
single command, `wsl --install --no-distribution`, in a UAC-elevated process
that the app waits for. When Windows needs a restart the app writes one per-user
`HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce` value
(`AgentifyResumeBootstrap`) that relaunches it with `--resume`, says "reboot
required" instead of retrying, and removes the value once setup succeeds. An
Agent can read the state; it cannot consent. The state machine
(`missing`/`consent_required`/`installing`/`reboot_required`/`resuming`/`ready`/
`failed`) lives in `crates/bootstrap`, with the elevated command, the restart
signal, and the registry injected so tests never install, reboot, or leave an
entry behind.

Caddy is not in the bundle yet. The daemon does not download it: ingress looks
for `%LOCALAPPDATA%\Agentify\tools\caddy.exe` (or `AGENTIFY_CADDY`) and reports
its typed `Missing` error until a pinned 2.10.2 binary is placed there.

### Updates

The shell updates itself through the Tauri updater plugin, and updates are built,
signed, and published by a GitHub Actions workflow that pushes the NSIS bundle
and the updater artifacts (`latest.json` + its signature) to a GitHub Release.
The endpoint in `ui/src-tauri/tauri.conf.json` points at that release.

To turn real updates on:

1. Generate a minisign keypair:
   `cd ui && npx tauri signer generate -w ~/.tauri/agentify-signing.key`
2. Add the private key as the `TAURI_SIGNING_PRIVATE_KEY` repository secret.
3. Paste the public key into `tauri.conf.json` under `plugins.updater.pubkey`
   (replacing the `<REPLACE_WITH_MINISIGN_PUBLIC_KEY>` placeholder).
4. Tag a release (`v0.1.1`) or run the `Release` workflow by hand.

Before an update applies, the shell asks the daemon whether a mutation is in
flight and refuses if one is. After the new daemon starts, its ordinary startup
adoption brings desired-running Environments back and reports any that failed to
adopt as degraded (`GET /v1/system/update_readiness` is the same gate).

### Uninstall

Uninstalling the app removes the login item, the trusted root, and Caddy data,
and leaves Projects and `state.db` untouched. The confirmed, destructive removal
is a separate command:

```sh
agentify uninstall            # non-destructive; Projects and state survive
agentify uninstall --purge    # also delete runtimes, Checkpoints, and the data root
```

## Data root and state

* Default: `%LOCALAPPDATA%\Agentify\` on Windows, `~/Library/Application Support/Agentify/`
  on macOS, `$XDG_DATA_HOME/Agentify/` elsewhere. Override with `AGENTIFY_HOME`.
* `state.db` — SQLite (bundled, WAL). Migrations run on start and are recorded in
  `schema_migrations`; the instance id is minted once per data root. Projects,
  their one `dev` Environment (desired and observed state plus health),
  Resources, each Project's normalized spec, the machine-wide `port_leases`,
  `operations`, and `idempotency_records` live here.
* `projects/<project-id>/workspace/` — the daemon-owned source directory for a
  Project. A bundled fixture is copied here from inside the binary, so an
  installed daemon needs no repository paths.
* Secret **values** never touch the data root: they live in the OS credential
  manager (Windows Credential Manager, Keychain, Secret Service), filed under the
  service name in `AGENTIFY_SECRET_SERVICE`. `state.db` holds metadata only —
  name, provenance, timestamps — in `secrets_metadata`, so the database is safe to
  copy. A test-configured daemon (`AGENTIFY_SECRET_STORE=memory`) mirrors its
  store to `test-secret-store.json` so a restart test keeps its values; the
  shipped daemon never writes that file.
* `discovery.json` — the loopback port, URL, and an Agent-scoped token. Written
  atomically and readable only by the current OS user (on Windows the DACL is
  replaced with a single ACE for that user). Never contains a human-capable token.
* Agent tokens live in the database as SHA-256 hashes only. Each daemon run mints
  a fresh token and revokes the previous one, so the discovery file always holds
  the only live token.

`desired_state` is what the last mutation asked for, `observed_state` is what the
daemon last saw. They diverge across a restart on purpose: a runtime that did not
survive is reported `degraded` rather than quietly claimed `running`, and the
next `start` converges it back on the same leases.

## API (v1)

| Route | Auth | Notes |
| --- | --- | --- |
| `GET /v1/healthz` | none | liveness; used by clients to detect a stale discovery file |
| `GET /v1/whoami` | Agent token | project context (`null` = global), scopes, daemon and API versions, the active Backend, and its capabilities |
| `GET /v1/events` | Agent token | SSE heartbeats |
| `POST /v1/projects` | Agent token | exactly one of `{"fixture": "hello-pg"}` or `{"manifest": "<yaml>"}`; `201` with the Project |
| `GET /v1/projects` | Agent token | `{"projects": [...]}`, oldest first |
| `GET /v1/projects/{project}` | Agent token | by name or id |
| `POST /v1/projects/{project}/environments/dev/start` | Agent token | idempotent and convergent; returns the status once every Resource is up, or `504` with an `operationId` |
| `POST /v1/projects/{project}/environments/dev/stop` | Agent token | idempotent; keeps the runtime and volumes, releases the leases |
| `GET /v1/projects/{project}/environments/dev/status` | Agent token | state, health, Resources, leased ports, Backend |
| `POST /v1/projects/{project}/environments/dev/resources/{resource}/exec` | Agent token | `{"command": [...], "timeoutMs": 5000}` → `{stdout, stderr, exitCode, durationMs}` |
| `GET /v1/projects/{project}/environments/dev/logs` | Agent token | bounded `tail`, optional `since` and `resource`; `follow=1` is SSE |
| `GET /v1/projects/{project}/environments/dev/stats` | Agent token | environment-level cpu/memory/disk |
| `GET /v1/projects/{project}/secrets` | Agent token | metadata only: name, provenance (`generate`, `fromEnv`, `literal`), whether the Manifest references it |
| `POST /v1/projects/{project}/secrets` | Agent token | `{"name": "...", "generate": "password"\|"token"}` or `{"name": "...", "value": "..."}`; the value is stored and never echoed |
| `PATCH /v1/projects/{project}/secrets/{name}` | Agent token | replace a value; an imported (`fromEnv`) secret is `SecretImmutable` |
| `DELETE /v1/projects/{project}/secrets/{name}` | Agent token | refused with `SecretInUse` while the Manifest references it |
| `POST /v1/projects/{project}/secrets/{name}/reveal` | Agent token + OS gesture | the one route that returns a value; audited without it |
| `GET /v1/operations/{id}` | Agent token | a mutation's operation |
| `GET /v1/operations/{id}/wait` | Agent token | block until it finishes (`?timeoutMs=`) |

A mutating call blocks for at most `AGENTIFY_OPERATION_TIMEOUT_MS`; a timeout is
a `504` whose problem body carries the `operationId`, so a caller waits instead
of retrying blindly. Mutations are serialised per Environment. `Idempotency-Key`
on a mutation replays the original response for 24 hours without running it
again.

State is `absent`, `starting`, `running`, `degraded`, `stopped`, or `failed`.
Health is `healthy`, `unhealthy`, or `unknown`, and it is the daemon's decision:
it probes the host ports it leased.

Errors are `application/problem+json` with a stable `code`: the Backend enum
(`Unsupported`, `VersionMismatch`, `NotProvisioned`, `AlreadyExists`,
`PortUnavailable`, `ExecutionFailed`, `BackendUnavailable`, `Timeout`,
`Cancelled`) plus `NotFound`, `ValidationFailed` (with a `path`),
`SecretSourceMissing`, `SecretInUse`, `SecretImmutable`, and `ApprovalRequired`.
Everything binds to `127.0.0.1` only.

A Project's known secret values, and their percent-encoded forms, are redacted
from every response body, log line, exec output, and audit entry — the reveal
route is the single deliberate exception.

## Manifest v0

`version`, `project`, and `resources` are required; the schema is closed, so an
unknown field is a rejection at every level. An `app` has exactly one of `image`
or `build`; a `database` is Postgres with a `${secret:name}` password; one port may
default its hostname to the Resource name while more than one requires every
hostname; hostnames are unique per Project; dependency cycles, self-dependencies,
and unknown dependencies are rejected; a build context must stay inside the
workspace. See `prototype/manifest-v0/agentify.yaml` for the annotated shape and
`fixtures/hello-pg/agentify.yaml` for the bundled example.

## Test

```sh
cargo test --workspace     # Backend conformance, manifest unit tests, and daemon, MCP, and CLI acceptance tests
cargo build --workspace
cd ui && npm test          # renderer states: header, rail, detail, status bar, Start/Stop, logs, empty, unreachable
cd ui/src-tauri && cargo check --all-targets && cargo test
```

No automated test touches this machine's real credentials: the daemon's secret
store is a seam, and every suite runs it against the in-memory one. The real OS
credential manager is exercised by one opt-in test, which uses a per-run service
name and deletes what it writes:

```sh
AGENTIFY_TEST_KEYRING=1 cargo test -p agentify-daemon --lib secrets
```

Two seams, as the spec agreed:

1. **Backend contract** — `crates/backends/src/conformance.rs` is one table of
   cases every adapter must pass: lifecycle idempotence, convergence after
   removal, exec streaming and exit codes, cancellation, log tailing, port
   binding, capability reporting, and typed unsupported errors. The fake runs it
   on every OS today; the WSL adapter will run it on Windows.
2. **Public API** — acceptance tests boot the real daemon against a temporary
   data root, read the discovery file, and speak HTTP, SSE, JSON-RPC over stdio,
   or run the CLI. Both daemons in the restart test are real processes, so a
   restart really does lose the runtime.

No test asserts on private Rust internals, and the only substitute is the
scripted fake Backend.
