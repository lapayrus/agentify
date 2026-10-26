# Manifest v0 prototype

**Question:** what is the v0 shape of `agentify.yaml`?

Open `manifest-v0.html` by double-clicking it. It is a throwaway, self-contained demo: no build, no server, no tests. It parses nothing — the YAML panel is the human source, the right panel is the normalized result of the same manifest as a plain object. The point is the shape, not YAML syntax.

## What the prototype argues for

- **Strict.** Unknown fields are rejected. A hand-written manifest's main failure mode is a typo that silently does nothing.
- **Three required keys.** `version`, `project`, `resources`. The project name is the URL namespace, so it is never inferred from a folder.
- **Resources are a map, not a list.** Names are identity, and dependency references read naturally.
- **`app` needs exactly one of `image` or `build`.** Build is a context plus a Dockerfile; no framework detection in v0.
- **Secrets are named, never inline.** `generate: password|token` or `fromEnv: NAME`, referenced as `${secret:name}`. Values are daemon-owned.
- **Ports carry hostnames.** One port may default to the resource name; more than one requires every hostname, because there is no sensible default.
- **The daemon owns the noisy names.** Distro, volume, port lease, route, and generated secret values never appear in the manifest.

## Not settled here

- A `service` kind (Compose-shaped multi-container services).
- Per-environment overrides (`dev`, later `test`).
- Importing `devcontainer.json` or `docker-compose.yml`.
- Any `seed` or migration field: the slice runs migrations with `exec`, so the manifest does not need one yet.
