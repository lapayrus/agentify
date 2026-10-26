# Research: trust a local CA without admin

Type: research
Status: resolved
Blocked by: none

## Question

Can the app install trust for Caddy's internal CA without administrator rights on Windows 11 and macOS, and what do browsers actually honour?

Cover the Windows certificate stores (`certutil`, PowerShell `Import-Certificate`) for `CurrentUser\Root` versus `LocalMachine\Root` and which needs elevation; Chrome and Edge on Windows versus Firefox's own NSS store; Safari and the macOS keychain; `caddy trust` behaviour on each platform; whether per-user trust is enough for `https://*.localhost`; certificate lifetime and rotation; and clean removal on uninstall.

Recommend the least-privilege path that keeps `https://web.<project>.localhost` green in Chrome, Edge, Firefox, and Safari, and state exactly where a one-time elevation is unavoidable. Findings go to [`research/local-ca-trust.md`](../../../research/local-ca-trust.md).

## Answer

No elevation is needed on Windows, and macOS needs one user authentication prompt but no admin. Caddy's defaults are wrong for both goals and must be overridden.

- Windows: `certutil -addstore -user Root <root.crt>` writes `HKCU` with no elevation. Chrome and Edge read `CurrentUser\Root`. Firefox ignores the OS store by default; set `security.enterprise_roots.enabled` (policy `Certificates.ImportEnterpriseRoots`) so it enumerates `HKCU`.
- macOS: `caddy trust` is hardcoded to `sudo security add-trusted-cert -d -k /Library/Keychains/System.keychain`. Set Caddy's `install_trust false` and run `security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db <root.crt>` from the GUI once. Chrome and Safari honour the user domain.
- Root lifetime is hard-coded to 10 years; the intermediate renews every 7 days. Replacing the root means re-trusting every machine.
- Caddy issues `*.localhost` at any depth via `SubjectIsInternal`. A wildcard certificate covers only one label, so per-resource hostnames must be issued explicitly.
- Removal: `certutil -delstore -user Root "Caddy Local Authority"` and `security delete-certificate -c "Caddy Local Authority" -t login.keychain-db`, then delete the Caddy data directory.
- Refusal fallback: `http://<host>.localhost` remains a secure context because Chromium ignores HSTS for localhost, so the UI degrades rather than breaks.

Full detail: [`research/local-ca-trust.md`](../../../research/local-ca-trust.md).
