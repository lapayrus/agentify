# 06: Secrets

**What to build:** Project-scoped secrets stored in the OS credential manager, generated or imported once, referenced from Manifests, injected at runtime, and never returned by the API. Humans can reveal a value after an OS prompt; Agents cannot.

**Blocked by:** 02 Projects from a Manifest, 03 Environment lifecycle on the fake Backend.

**Status:** ready-for-agent

- [ ] Generated passwords and tokens are created by the daemon and stored in the OS keyring; nothing is written to disk in plaintext.
- [ ] `fromEnv` imports a value once at Project creation, falls back to the Windows user environment, and fails with `SecretSourceMissing` when absent.
- [ ] Manifest references resolve at start; the resolved values reach the Resource environment but never appear in status, logs, errors, or API responses.
- [ ] Known values and their percent-encoded forms are redacted from logs and exec output.
- [ ] A human reveal requires an OS authentication gesture and is audited without the value; an Agent token has no read path.
- [ ] Agents can create and update secrets and delete only unreferenced ones; deleting a referenced secret is rejected.
