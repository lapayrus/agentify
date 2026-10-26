# Define the Agent API and MCP surface v0

Type: grilling
Status: resolved
Blocked by: 03

## Question

What is the v0 Agent surface?

Decide the resource-oriented HTTP endpoints, the MCP tool names and descriptions, async job versus synchronous call semantics, idempotency keys, and the error shape.

Decide per-Project token scopes, which operations require an Approval, and the audit event schema. The API is the product surface, so every name must come from `GLOSSARY.md` and every UI action must exist as an API call.

Include how an Agent discovers what it is allowed to do, and what a human sees in the UI while an Agent is acting.

## Answer

REST over loopback HTTP/1.1, `/v1`, with SSE for events and log following and WebSocket for interactive exec. The MCP server is a stdio shim over the same API; there is no second source of truth.

Endpoints:

- Projects: `GET/POST /v1/projects`, `GET/DELETE /v1/projects/{project}`
- Environment: `GET /v1/projects/{project}/environments`, `POST .../{env}/start|stop|reset`, `GET .../{env}/status`
- Resources: `GET .../resources`, `POST .../resources/{resource}/exec`, `GET .../logs`, `GET .../stats`
- Checkpoints: `GET/POST .../checkpoints`, `POST .../checkpoints/{id}/restore`, `DELETE .../checkpoints/{id}`
- Secrets: `GET/POST .../secrets`, `PATCH/DELETE .../secrets/{name}` — values are never returned
- Operations: `GET /v1/operations/{id}`, `GET /v1/operations/{id}/wait`
- Events: `GET /v1/events` (SSE), filtered by Project
- Exec: `GET /v1/exec/{id}` (WebSocket)
- System: `GET /v1/whoami`

Mutating calls block until complete or until the request timeout. A timeout returns `504` with the `operation_id` so the caller can wait or subscribe. An operation needing an Approval returns `202` with `approval_id` and `operation_id`; the human approves in the inbox and the Agent waits on the operation. It never polls and retries.

Scopes are per-Project and coarse: `read`, `operate`, `admin`. `admin` operations still require Approval where destructive. Tokens are issued by a human through the CLI or UI. The daemon writes a user-only discovery file containing its loopback port and bearer token.

Approvals are single-use, bound to a hash of the operation and parameters, expire after 15 minutes, and only a human session can grant them. They gate `reset_environment`, `restore_checkpoint`, and `delete_project`.

MCP tools, verb-first:

- Projects: `list_projects`, `get_project`, `create_project`, `delete_project`
- Environment: `start_environment`, `stop_environment`, `get_status`, `reset_environment`
- Resources: `run_command`, `read_logs`, `get_stats`
- Checkpoints: `create_checkpoint`, `list_checkpoints`, `restore_checkpoint`
- Secrets: `list_secrets`, `create_secret`, `update_secret`, `delete_secret`
- Operations: `wait_for_operation`
- System: `whoami`

`create_project` takes exactly one of a bundled fixture (`hello-pg`) or an inline Manifest. A Manifest using `build` without a fixture fails with `SourceMissing` until file tools exist. `run_command` returns stdout, stderr, and exit code with no interactive stdin. `read_logs` returns a bounded tail by default.

Errors are `application/problem+json` with a stable `code`: the Backend enum (`Unsupported`, `VersionMismatch`, `NotProvisioned`, `AlreadyExists`, `PortUnavailable`, `ExecutionFailed`, `BackendUnavailable`, `Timeout`, `Cancelled`) plus `NotFound`, `ValidationFailed`, `SecretSourceMissing`, `SourceMissing`, `ApprovalRequired`, `ApprovalExpired`. HTTP status is a hint; the code is the contract. `Idempotency-Key` is supported on every mutation and replays the original response for 24 hours.

Audit entries are append-only: `{ id, at, actor: { kind: human|agent, id, token_id }, project_id, operation, target, params_hash, approval_id?, result, error_code?, duration_ms, request_id }`. Secret and environment values are excluded by construction.

A human sees every Agent action in the activity feed and operation status through the same SSE stream; `actor.kind` is the only difference. `whoami` is the discovery handshake: Project, scopes, daemon and API versions, capabilities.
