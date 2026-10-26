# Agentify

A local, agent-native environment manager. It gives each Project its own isolated runtime on the developer's machine and exposes that runtime to both AI agents and humans through one control plane.

## Language

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
