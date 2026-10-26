# One local daemon is the only control plane

The daemon owns all state and operations and listens only on loopback or a named pipe. The desktop UI, CLI, and MCP server are clients of the same API. Agent access uses per-project capability tokens, destructive operations require a recorded Approval, and every mutation is audited. This prevents the human path and the agent path from diverging, which is the failure mode of bolt-on AI features.

Consequences: the UI has no privileged path; anything the UI can do, an Agent can be granted; the API surface is a product surface and needs versioning.
