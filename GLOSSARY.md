# Agentify

A local, agent-native environment manager. It gives each Project its own isolated runtime on the developer's machine and exposes that runtime to both AI agents and humans through one control plane.

## Language

**Daemon**:
The process that owns Project state, port leases, health, Checkpoints, and the `/v1` API. It is app-scoped: the
shell or the MCP shim starts one on demand, and it stops when its owner exits. Never a login
item, a Scheduled Task, or a Windows service.
_Avoid_: server, backend (a Backend is the runtime substrate), service

**Stray daemon**:
A daemon a caller does not ship: a live process answering with another build's version, or a discovery file
naming a process that is gone. A caller stops a stray daemon before starting its own, and never drives it.
_Avoid_: orphan, zombie, stale process

**Project**:
A named workspace bound to one host directory that owns one or more Environments.
_Avoid_: workspace, app, repo

**Environment**:
An isolated runtime instance of a Project. v0 recognises only `dev`.
_Avoid_: stack, instance, stage

**Resource**:
An application, database, or service running inside an Environment.
_Avoid_: workload, container; "service" only for the Resource kind

**Backend**:
The runtime substrate that hosts an Environment: a WSL distro, an Apple container microVM, or a Lima VM.
_Avoid_: provider, destination, host, server

**Port lease**:
A host port on `127.0.0.1` the daemon reserves for one guest port of one Resource, and releases on stop. The
daemon owns the registry; a Backend binds what it is leased and never invents a port.
_Avoid_: port mapping, port forward, assigned port

**Operation**:
A mutation the daemon runs under an id, so a caller whose HTTP call timed out can wait for its outcome instead
of retrying.
_Avoid_: job, task, request

**Manifest**:
The `agentify.yaml` file that declares a Project's desired state.
_Avoid_: config, spec, compose file

**Agent**:
An AI process authenticated against the local daemon with a project-scoped token.
_Avoid_: bot, assistant, client

**Approval**:
A recorded human decision that authorises one destructive operation.
_Avoid_: confirmation, permission, prompt

**Checkpoint**:
A restorable copy of an Environment's data volumes at a point in time.
_Avoid_: snapshot (reserve for backend-native mechanisms), backup
