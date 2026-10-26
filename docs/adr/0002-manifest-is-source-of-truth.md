# The Project manifest is the source of truth

We decided `agentify.yaml` is the authoritative desired state for a Project, and `devcontainer.json` and Docker Compose are import/export adapters rather than the native format. Adopting someone else's spec as our core would tie our lifecycle, networking, and data semantics to their evolution; adapters keep ecosystem compatibility without that coupling.

Consequences: the manifest needs a versioned schema and a migration path; devcontainer and Compose imports are lossy by design and must say what they dropped.
