# Define secrets and environment handling

Type: grilling
Status: resolved
Blocked by: 02

## Question

Where do secrets live, how do they reach a Resource, and what can an Agent see?

Decide the store (OS credential manager versus an encrypted file), host-to-guest injection, build-time versus runtime values, and redaction in logs and API responses.

Decide the Agent boundary explicitly: can an Agent read a plaintext secret, or only reference it by name? Can it create one? What appears in the audit log when it does? Tie this to the per-Project token scope from ADR-0003.

## Answer

The enforceable boundary is the API and the Project, not the Agent's own Environment.

- Store: the OS credential manager through a cross-platform keyring (Windows Credential Manager, macOS Keychain). No plaintext at rest. An unavailable store fails with a typed error.
- Scope: Project-scoped in v0, shared across Environments. Per-Environment overrides are deferred.
- Injection: runtime environment variables only. No build-time secrets; builds must not require them.
- Agent boundary: the API never returns plaintext. An Agent that executes code in an Environment can read whatever that Resource can read, so the guarantee is Project-to-Project isolation plus audit, not "the Agent cannot see it". `exec` inherits the Resource's environment.
- Agents may create and update secrets (values supplied by the Agent, audited) and delete only secrets no Manifest references. Reading plaintext is human-only.
- Human reveal: CLI or UI with an OS authentication prompt, audited, never through the Agent API.
- `fromEnv`: imported once at Project creation from the daemon's environment, with `HKCU\Environment` as the Windows fallback for GUI-launched daemons, then immutable until rotation. A missing source fails creation with `SecretSourceMissing`.
- Redaction: exact values and their percent-encoded forms are removed from logs, exec output, status, errors, and audit entries. Best-effort, never a security boundary.
- Rotation is not in v0; it needs per-engine work such as Postgres `ALTER USER` plus a Resource restart.
