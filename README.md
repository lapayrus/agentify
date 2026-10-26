# Agentify

A local, agent-native environment manager. Each Project runs in its own isolated
runtime on the developer's machine, and one control plane exposes it to agents
and humans alike. Read `GLOSSARY.md` for the domain language, `docs/adr/` for the
hard-to-reverse decisions, and `.scratch/agentify-v0/spec.md` for the product spec.

**Current status: ticket 01, the walking skeleton.** There is a daemon with
state, a loopback API, MCP `whoami`, a CLI, and a desktop shell that shows the
header, an empty Project rail, and a clear empty state. There are no Projects,
Environments, Resources, Backends, or ingress yet — those arrive in tickets 02+.

## Layout

| Path | What it is |
| --- | --- |
| `crates/proto` | Wire types, discovery-file format, data-root resolution, and the API client shared by every process |
| `crates/daemon` | `agentifyd`: owns state (SQLite), serves `/v1/whoami` and `/v1/events` on loopback |
| `crates/mcp` | `agentify-mcp`: MCP stdio shim exposing `whoami` over JSON-RPC |
| `crates/cli` | `agentify`: `daemon`, `status`, `whoami` |
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

`initialize`, `tools/list`, and `tools/call` for `whoami` work today. The
`whoami` result is byte-for-byte the payload of `GET /v1/whoami`.

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
  `schema_migrations`; the instance id is minted once per data root.
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

Errors are `application/problem+json` with a stable `code`. Everything binds to
`127.0.0.1` only.

## Test

```sh
cargo test --workspace     # daemon unit tests + API-seam, MCP, and CLI acceptance tests
cargo build --workspace
cd ui && npm test          # renderer states: header, rail, empty, unreachable
cd ui/src-tauri && cargo check --all-targets
```

Tests drive real processes at the API seam: they boot `agentifyd` against a
temporary data root, read the discovery file, and speak HTTP, JSON-RPC over
stdio, or run the CLI. No test asserts on private Rust internals.
