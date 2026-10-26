# 14: Cross-Project isolation and security acceptance

**What to build:** The security guarantees are exercised as observable behaviour: scoped and expiring tokens, abuse caps, no privileged requests, redaction, path safety, and an audit chain that detects tampering.

**Blocked by:** 10 Durability and isolation, 13 Updates and uninstall.

**Status:** ready-for-agent

- [ ] Agent tokens expire after seven days, can be revoked, and are rejected after revocation.
- [ ] The 20-Project and two-concurrent-start caps are enforced with typed errors, and disk pressure produces a warning.
- [ ] A Manifest requesting privileged mode, host networking, host PID, or a Docker socket is rejected at parse time and never reaches a Backend.
- [ ] Cross-Project probes fail: no volume read, no database authentication, no hostname resolution.
- [ ] An Agent token cannot approve an Approval; expired, replayed, and parameter-mismatched Approvals are rejected.
- [ ] Path traversal through copy is rejected; known secret values are redacted from logs and exec output.
- [ ] The audit chain verifies on read, and tampering with an entry is detected.
- [ ] The security suite runs green on Windows and is wired into CI with the fake Backend.
