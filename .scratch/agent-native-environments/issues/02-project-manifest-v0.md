# Define the Project manifest v0

Type: prototype
Status: resolved
Blocked by: none

## Question

What is the v0 shape of `agentify.yaml`?

Produce a concrete sample for the vertical slice and decide: required versus inferred fields, how Resources are declared (image, build, services), environment and secret references, port and hostname declarations, volume declarations, and which parts the daemon generates versus what the user writes.

The answer is the schema plus one annotated example. Include a `version` field and state how unknown fields are treated. Keep it small enough that a person can hand-write it.

## Answer

Verdict: the shape in [`prototype/manifest-v0/manifest-v0.html`](../../../prototype/manifest-v0/manifest-v0.html) is right.

- Required top level: `version: 1`, `project`, and `resources` (a map with at least one entry).
- `secrets`: name → `{ generate: password|token }` or `{ fromEnv: NAME }`. Values are daemon-owned; manifests reference them as `${secret:name}`. v0 has no `env` interpolation and no inline secret values.
- `app`: `kind` plus exactly one of `image` or `build` (`context` + `dockerfile`); optional `command` (string array), `env` (literals and secret references), `ports`, `dependsOn`, and `healthcheck`.
- `database`: `kind`, `engine: postgres`, optional `version` (default 16), `credentials` with a username and a secret-ref password, and an optional `volume` name.
- Ports are `{ port, hostname? }`. A single port defaults to the resource name; more than one requires every hostname. Routes are `<hostname>.<project>.localhost` and unique within the Project.
- Unknown fields are rejected at every level. Typos fail loudly.
- The daemon generates the runtime distro name, project id, volume names (`<project>-<resource>-data`), port leases, Caddy routes, generated secret values, and a TCP healthcheck when an app declares ports but no healthcheck.

Annotated example: [`prototype/manifest-v0/agentify.yaml`](../../../prototype/manifest-v0/agentify.yaml). Rationale and open items: [`prototype/manifest-v0/README.md`](../../../prototype/manifest-v0/README.md).

Left to later tickets: a `service` kind, per-environment overrides, and importing devcontainer or Compose.
