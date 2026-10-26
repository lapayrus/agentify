# Define the security threat model

Type: grilling
Status: resolved
Blocked by: 07

## Question

What is the threat model with an Agent as a principal?

Decide host mount policy (can a Project see the host project directory, and read-write?), default network egress, and protection against a malicious Manifest or service template.

Cover secret exfiltration paths, token theft, Approval bypass, and what the audit log must capture to reconstruct an incident. Distinguish risks from a mistaken Agent from risks from a hostile Agent or a compromised dependency, because the mitigations differ.

The Backend contract exposes `copyIn`/`copyOut` only, with daemon-owned host paths; host mounts are a capability and their policy is this ticket's decision.

## Answer

**Boundary statement.** Same-user processes are inside the trust boundary: they can call the daemon API, read the discovery file, and reconfigure Caddy through its admin port. On Windows, a root inside one distro can reach another Project's loopback listeners because the kernel and network namespace are shared. We do not claim kernel isolation on Windows.

**Enforceable guarantees**: Project-scoped tokens, OS-authenticated Approvals, no host mounts, separate volumes, separate database credentials, loopback-only ingress, and an append-only hash-chained audit.

- Approvals require an interactive OS gesture in the UI (Windows Hello, macOS LocalAuthentication) producing a short-lived assertion bound to the Approval id. Agent tokens can request but never approve.
- The discovery file holds an Agent-scoped token, never a human-capable one, with a user-only ACL.
- Project tokens expire after seven days by default and are revocable.
- Outbound network is allowed; inbound is loopback only.
- A Manifest cannot request privileged mode, host networking, host PID, or a Docker socket mount. The daemon rejects them at parse time and the Backend refuses to pass them through.
- No host mounts in v0. `copyIn`/`copyOut` accept only daemon-owned workspace paths.
- Images may use tags; digests are supported; no signature verification in v0. This is an accepted risk.
- The audit is append-only and hash-chained, and reads verify the chain. It is detectable, not tamper-proof.
- Abuse caps: 20 Projects and two concurrent starts, with a disk-usage warning. Per-Project CPU and memory budgets stay in the fog.
- Checkpoints are plain files in the app-data root with user-only ACLs; encryption at rest is fog.
- Secrets are Project-scoped. An Agent running code inside a Project can read that Project's secrets, never another Project's.
- Malicious Manifest or build code runs only inside the Project runtime with no host access. Build-time secrets are unsupported.
- Update integrity comes from signed installers and signed updates (ticket 11).
- Destructive operations remain Approval-gated and audited, and the audit excludes values.
