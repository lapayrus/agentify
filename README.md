# Agentify

A local, agent-native environment manager. Each Project runs in its own isolated
runtime on the developer's machine, and one control plane exposes it to agents
and humans alike. Read `GLOSSARY.md` for the domain language, `docs/adr/` for the
hard-to-reverse decisions, and `.scratch/agentify-v0/spec.md` for the product spec.

**Current status: ticket 02, Projects from a Manifest.** There is a daemon with
state, a loopback API, an MCP server, a CLI, and a desktop shell. Projects can be
created from the bundled `hello-pg` fixture or an inline Manifest and inspected
through the API, MCP, and the UI. Nothing starts yet — no Backends, ingress,
secrets store, or Approvals.

## Layout

| Path | What it is |
| --- | --- |
| `crates/proto` | Wire types, discovery-file format, data-root resolution, and the API client shared by every process |
| `crates/manifest` | Pure Manifest parse and normalization into a spec: strict, path-accurate errors, no I/O |
| `crates/daemon` | `agentifyd`: owns state (SQLite), serves `/v1` on loopback, embeds the bundled fixtures |
| `crates/mcp` | `agentify-mcp`: MCP stdio shim exposing `whoami`, `create_project`, `list_projects`, and `get_project` |
| `crates/cli` | `agentify`: `daemon`, `status`, `whoami` |
| `fixtures/` | Bundled fixtures compiled into the daemon (`hello-pg`) |
| `ui/` | Desktop front end: plain HTML/CSS/JS modules, built into `ui/dist` |
| `ui/src-tauri` | Tauri v2 shell crate (its own workspace; embeds `ui/dist`) |

The daemon is the only control plane (ADR-0003). The shell, the CLI, and the MCP
shim are clients of the same API; none of them has a privileged path.

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

`initialize`, `tools/list`, and `tools/call` work today for `whoami`,
`create_project`, `list_projects`, and `get_project`. Every tool result is
byte-for-byte the payload of the matching HTTP route:

```json
{"name": "create_project", "arguments": {"fixture": "hello-pg"}}
{"name": "create_project", "arguments": {"manifest": "version: 1\nproject: demo\n..."}}
{"name": "list_projects", "arguments": {}}
{"name": "get_project", "arguments": {"project": "hello-pg"}}
```

A tool failure is a JSON-RPC error whose `data` carries the same stable `code`
the API returns (`ValidationFailed`, `AlreadyExists`, `SourceMissing`,
`NotFound`), plus `path` for a validation failure.

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

## Data root and state

* Default: `%LOCALAPPDATA%\Agentify\` on Windows, `~/Library/Application Support/Agentify/`
  on macOS, `$XDG_DATA_HOME/Agentify/` elsewhere. Override with `AGENTIFY_HOME`.
* `state.db` — SQLite (bundled, WAL). Migrations run on start and are recorded in
  `schema_migrations`; the instance id is minted once per data root. Projects,
  their one `dev` Environment, Resources, and each Project's normalized spec live
  here.
* `projects/<project-id>/workspace/` — the daemon-owned source directory for a
  Project. A bundled fixture is copied here from inside the binary, so an
  installed daemon needs no repository paths.
* `discovery.json` — the loopback port, URL, and an Agent-scoped token. Written
  atomically and readable only by the current OS user (on Windows the DACL is
  replaced with a single ACE for that user). Never contains a human-capable token.
* Agent tokens live in the database as SHA-256 hashes only. Each daemon run mints
  a fresh token and revokes the previous one, so the discovery file always holds
  the only live token.

## API (v1)

| Route | Auth | Notes |
| --- | --- | --- |
| `GET /v1/healthz` | none | liveness; used by clients to detect a stale discovery file |
| `GET /v1/whoami` | Agent token | project context (`null` = global), scopes, daemon and API versions, capabilities |
| `GET /v1/events` | Agent token | SSE heartbeats |
| `POST /v1/projects` | Agent token | exactly one of `{"fixture": "hello-pg"}` or `{"manifest": "<yaml>"}`; `201` with the Project |
| `GET /v1/projects` | Agent token | `{"projects": [...]}`, oldest first |
| `GET /v1/projects/{project}` | Agent token | by name or id |

A Project carries its one `dev` Environment, its Resources, the derived Routes
(`https://<hostname>.<project>.localhost`), and state and cost placeholders —
nothing runs yet.

Errors are `application/problem+json` with a stable `code`: `ValidationFailed`
(with a `path`), `AlreadyExists`, `SourceMissing`, `NotFound`. Everything binds to
`127.0.0.1` only.

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
cargo test --workspace     # manifest unit tests + daemon, MCP, and CLI acceptance tests
cargo build --workspace
cd ui && npm test          # renderer states: header, populated rail, detail, empty, unreachable
cd ui/src-tauri && cargo check --all-targets
```

Tests drive real processes at the API seam: they boot `agentifyd` against a
temporary data root, read the discovery file, and speak HTTP, JSON-RPC over
stdio, or run the CLI. No test asserts on private Rust internals.
