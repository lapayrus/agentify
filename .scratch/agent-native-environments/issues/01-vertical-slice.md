# Define the first vertical slice

Type: grilling
Status: resolved
Blocked by: none

## Question

What exactly must the first vertical slice prove, and what are its acceptance checks?

Candidate: a Node 22 app plus Postgres 16 in one Project. An Agent creates the Project and Environment through the API and MCP server, runs a migration, and starts the app. A human opens it at `web.<project>.localhost` over trusted TLS. Database data survives a daemon restart. A Checkpoint restores it. Teardown leaves no distro, volume, or Caddy route behind.

Decide the framework and database, and which checks are in v0 versus later. The slice is the definition of done for the first implementation effort, so it must be small enough to finish and broad enough to exercise isolation, ingress, secrets, and data.

## Answer

Agent-first end to end through MCP; the human observes and approves. A second Project exists only as a leakage assertion, never as a second story.

Slice contents: Node 22 + Postgres 16, deployed from a bundled fixture at `examples/hello-pg` (app, migration, manifest). The app is served at `https://web.hello.localhost` with a trusted certificate. The human surface is a read-only dashboard with live logs and an Approval inbox; no editor, terminal, or secret UI. A scripted MCP client gates the slice; one real agent session repeats it manually.

Data: survive daemon restart, reset, Checkpoint restore, and complete teardown. Checkpoint is database-native (`pg_dump` / `CREATE DATABASE … TEMPLATE`), covers database Resources only in v0, and is exposed as `checkpoint.create(environment)`.

Trust: Approvals gate `environment.reset`, `checkpoint.restore`, and `project.delete`; create, start, exec, and logs are ungated. Secrets are one daemon-generated Postgres password in the OS credential manager plus one user-supplied env var; both are referenceable by an Agent, the API never returns plaintext, and the enforceable boundary is Project-to-Project isolation plus audit. Every mutation is audited with actor, token, operation, and target.

Isolation is asserted at the data, filesystem, and hostname level: Project A cannot read B's volumes, authenticate to B's database, or resolve B's hostname. Listener invisibility is not asserted because Windows distros share a network namespace.

Hostnames are `<resource>.<project>.localhost`, only for Resources that declare a port; there is no Project apex.

Acceptance platform: Windows 11. The Backend contract must remain platform-neutral.

Definition of done:

1. A scripted MCP client calls `project.create` with the fixture; the Project appears with one `dev` Environment and two Resources (app, database).
2. `environment.up` returns after health checks pass and the app is reported healthy.
3. `https://web.hello.localhost` returns 200 over a trusted certificate and renders a row read from Postgres.
4. `exec` runs the fixture migration and the row appears.
5. Restarting the daemon leaves the row intact and the URL serving.
6. `environment.reset` blocks on an Approval; after approval the table is empty.
7. `checkpoint.create` → mutate → `checkpoint.restore` (approved) brings the row back.
8. With a second Project up, A cannot read B's volumes, authenticate to B's database, or resolve B's hostname.
9. `project.delete` blocks on an Approval; after approval the distro, volumes, Caddy route, and port lease are gone, and `wsl -l -v` shows no leftover.
10. Every mutation appears in the audit log with actor, token, operation, and target.
