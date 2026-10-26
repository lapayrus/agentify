# 09: The vertical slice: hello-pg end to end

**What to build:** The bundled Node 22 app and Postgres 16 run for real. An Agent creates the Project, starts it, runs the migration, and opens `https://web.hello.localhost`; the page renders a row read from Postgres through a generated password.

**Blocked by:** 04 Local ingress, 05 Approvals/audit, 06 Secrets, 08 WSL exec/logs/stats/copy.

**Status:** ready-for-agent

- [ ] The fixture builds and starts as the `web` Resource; Postgres starts as `db` with a generated password and a named volume.
- [ ] `environment.up` returns only after health checks pass and the app is reported healthy.
- [ ] `https://web.hello.localhost` returns 200 over a trusted certificate and renders a row from Postgres.
- [ ] `exec` runs the fixture migration and the row appears.
- [ ] The scripted MCP client passes acceptance checks 1–4.
- [ ] The UI shows the running Project, both Resources, the URL, and live logs.
