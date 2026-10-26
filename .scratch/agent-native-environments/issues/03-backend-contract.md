# Define the Backend contract

Type: grilling
Status: resolved
Blocked by: 08, 09, 13

## Question

What is the smallest Backend contract every substrate must satisfy so the daemon does not leak platform specifics?

Decide the operation set: create and destroy, start and stop, exec with streamed output and a PTY, file copy, port publish, logs, stats, and checkpoint. Add capability flags for operations a Backend cannot perform, and an error model for "not supported" that the UI and Agents can reason about.

Map a per-Project runtime lifecycle onto each Backend, including what "stopped" means and what survives it. The contract should be expressed in the glossary's vocabulary and should not name WSL, Lima, or Apple container.

## Answer

The contract is **environment-scoped** and lifecycle-only. A Backend hides whether an Environment is one distro with several containers, a set of per-container microVMs, or a VM.

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

Port publishing is not an operation: the daemon leases host ports, puts them in the `EnvironmentSpec`, and the Backend binds them during `start`.

Rules:

- `start`, `stop`, and `remove` are idempotent. `start` is convergent and re-creates missing workloads.
- The Backend owns restart policy inside the runtime. The daemon reissues `start` on drift and consumes events.
- The daemon serialises mutations per `EnvId`; read calls may run concurrently.
- Every long call accepts cancellation. A cancelled call leaves a recoverable state and returns `Cancelled`.
- Host port leases are released on `stop` and reacquired on `start`. Hostnames are stable; host ports are not.
- Host paths passed to `copyIn`/`copyOut` are daemon-owned workspace paths, never arbitrary user input.
- Secrets are resolved by the daemon and passed in the spec. The Backend redacts them from logs, status, and errors, and never returns them.
- `describe().version` is checked at handshake against a supported major. A mismatch fails with `VersionMismatch`.

Readiness: `start` returns when every Resource is running. The daemon probes health through the leased host ports and owns the `environment.up` decision.

Status states: `absent`, `provisioning`, `stopped`, `starting`, `running`, `degraded`, `failed`.

Backend events: `state_changed`, `resource_exited`, `lease_lost`, `backend_unavailable`. Health is a daemon-level event, not a Backend event. Events are an optional capability; without them the daemon polls `status`.

Error enum: `Unsupported`, `VersionMismatch`, `NotProvisioned`, `AlreadyExists`, `PortUnavailable`, `ExecutionFailed`, `BackendUnavailable`, `Timeout`, `Cancelled`. Raw stderr is diagnostic only.

Capability descriptor: `pty`, `mount`, `checkpoint`, `events`, `resourceStats`, `logSince`, plus backend name, version, and platform. A missing capability yields a typed `Unsupported`, never a surprise.

Checkpoint is capability-gated and unused by the v0 slice; Windows reports `Unsupported`.
