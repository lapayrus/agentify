# Browser-trusted HTTPS for `https://*.localhost` via Caddy's internal CA (Windows 11 + macOS)

Research ticket: can a desktop app give its users browser-trusted `https://web.hello.localhost` using Caddy's internal
CA, **without admin rights where possible**? Primary sources only (Microsoft Learn, Apple developer docs, Mozilla/NSS +
Firefox source/docs, Caddy docs + Caddy/smallstep source, Chromium source/docs). Commands are copy-pasteable; every
non-obvious claim is cited.

Platform assumptions: Windows 11 (user session, no elevation), macOS (GUI login session, standard/non-admin user with
admin credentials available on request). Caddy 2.10/2.11-era (`caddypki`, smallstep truststore).

---

## 0. Verdict up front

| | Windows 11 | macOS |
|---|---|---|
| Caddy's own `caddy trust` target store | **`CurrentUser\Root` — no elevation** | **admin domain / `System.keychain` — `sudo` required** |
| Least-privilege path | already per-user (nothing to change) | **skip `caddy trust`; install into the *user* trust domain** |
| User approval needed | none (silent HKCU write) | one GUI authentication panel (password/Touch ID) — unavoidable for trust changes |
| Chrome / Edge padlock | ✅ | ✅ |
| Safari | n/a (Safari for Windows is EOL) | ✅ |
| Firefox | ❌ by default; ✅ after `security.enterprise_roots.enabled`/policy or NSS import | same |

The single most useful finding: **on Windows, Caddy already installs per-user, not machine-wide**, and both Chromium
browsers honour `CurrentUser\Root`. On macOS, Caddy takes the *admin* path only because smallstep's darwin backend
hardcodes `sudo security add-trusted-cert -d`; an app can instead write the **user** trust domain and get the same
result for Safari/Chrome/Edge with no admin rights at all.

---

## 1. Windows certificate stores

### 1.1 Where the stores are

* Local machine store = `HKEY_LOCAL_MACHINE\Software\Microsoft\SystemCertificates`; current user store =
  `HKEY_CURRENT_USER\Software\Microsoft\SystemCertificates`.
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/local-machine-and-current-user-certificate-stores>
* The `Root` *system* store is a collection of physical sibling stores. `CERT_SYSTEM_STORE_CURRENT_USER\Root` =
  `.Default` + `.LocalMachine` + `.SmartCard`; `CERT_SYSTEM_STORE_LOCAL_MACHINE\Root` = `.Default` + `.AuthRoot` +
  `.GroupPolicy` + `.Enterprise` + `.SmartCard`.
  <https://learn.microsoft.com/en-us/windows/win32/seccrypto/system-store-locations>
* Current-user stores inherit the machine stores (`Trusted Root` semantics), so a machine-wide root is visible
  per-user — but not the reverse.
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/local-machine-and-current-user-certificate-stores>

### 1.2 Exact commands and elevation

`certutil` (verb then options; `-user` = "Use the HKEY_CURRENT_USER keys or certificate store"):
<https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/certutil>

```bat
:: per-user (NO elevation)
certutil -addstore -user Root  "%LOCALAPPDATA%\Caddy\pki\authorities\local\root.crt"
:: machine-wide (needs an elevated process)
certutil -addstore        Root  "...\root.crt"

:: remove
certutil -delstore -user Root "Caddy Local Authority"        :: by CN, or use the SHA-1 thumbprint
certutil -store    -user Root                                 :: list what is in HKCU\Root
```

PowerShell `Import-Certificate` (pki module): <https://learn.microsoft.com/en-us/powershell/module/pki/import-certificate>

```powershell
Import-Certificate -FilePath .\root.crt -CertStoreLocation Cert:\CurrentUser\Root   # no elevation
Import-Certificate -FilePath .\root.crt -CertStoreLocation Cert:\LocalMachine\Root  # elevation required
Get-ChildItem Cert:\CurrentUser\Root | Where-Object Subject -like '*Caddy Local Authority*' | Remove-Item
```

Which require elevation: **`LocalMachine\*` writes do** (the HKLM hive is admin-only for `SystemCertificates`);
**`CurrentUser\*` writes do not**. Windows' own API docs make the same distinction structurally — `CertOpenSystemStore`
"Only current user certificates are accessible using this method, not the local machine store."
<https://learn.microsoft.com/en-us/windows/win32/api/wincrypt/nf-wincrypt-certopensystemstorea>
A per-user root install is a plain Crypt32 write: no UAC prompt, no GUI warning (the "Security Warning" dialog belongs to
the `certmgr`/`rundll32` import *wizard*, not to `certutil`/API writes).

### 1.3 What Chrome and Edge read

* **Chrome** (Chrome Root Store verifier, macOS/Windows since Chrome 105/108) consumes locally-added certs from, on
  Windows: Local Machine → `Trusted Root Certification Authorities`, `Trusted People`, and the
  Enterprise/Group-Policy variants; **Current User → `Trusted Root Certification Authorities`** (+ Current-User
  Group-Policy, and Untrusted for distrust). It explicitly does *not* treat the platform's *default* root list as
  trust — only what a user/enterprise put there.
  <https://chromium.googlesource.com/chromium/src/+/main/net/data/ssl/chrome_root_store/faq.md>
* **Edge**: since Edge 112 the default CTL + verifier ship with the browser, but "the browser queries the underlying
  platform for—and trusts—locally installed roots that users and/or enterprises installed. As a result, scenarios where a
  user or enterprise installed more roots to the host operating system's root store should continue to work."
  <https://learn.microsoft.com/en-us/deployedge/microsoft-edge-security-cert-verification>
* Practical consequence: **`CurrentUser\Root` is sufficient for Chrome and Edge** — no elevation, no machine-wide trust.

### 1.4 Firefox

Firefox does **not** use the Windows store by default: it keeps its own NSS databases (`cert9.db`/`key4.db`) per profile.
Three non-interactive provisioning routes:

1. **Enterprise-roots bridge (recommended).** `security.enterprise_roots.enabled=true`, exposed as policy
   `Certificates.ImportEnterpriseRoots` (Windows GPO key
   `Software\Policies\Mozilla\Firefox\Certificates\ImportEnterpriseRoots`, or `policies.json` in `distribution\` beside
   the EXE). Documented as "Trust certificates that have been added to the operating system certificate store by a user
   or administrator"; Windows + macOS only.
   <https://firefox-admin-docs.mozilla.org/reference/policies/certificates/> (mirror: <https://mozilla.github.io/policy-templates/#certificates--importenterpriseroots>)
   Firefox's Windows implementation enumerates `CERT_SYSTEM_STORE_LOCAL_MACHINE`, `..._LOCAL_MACHINE_GROUP_POLICY`,
   `..._LOCAL_MACHINE_ENTERPRISE`, **`CERT_SYSTEM_STORE_CURRENT_USER`** and `..._CURRENT_USER_GROUP_POLICY` — i.e. a
   per-user (HKCU) root **is** picked up.
   <https://github.com/mozilla-firefox/firefox/blob/main/security/manager/ssl/EnterpriseRoots.cpp>
   Note it deliberately skips certs that came from Microsoft's root program, only user/admin-installed ones.
2. **NSS import with `certutil -d sql:`** — per profile, no OS involvement:
   ```bash
   certutil -d sql:"$HOME/.mozilla/firefox/<profile>" -A -t "C,," -n "Caddy Local Authority" -i root.pem
   certutil -d sql:"$HOME/.mozilla/firefox/<profile>" -L -n "Caddy Local Authority"     # verify
   certutil -d sql:"$HOME/.mozilla/firefox/<profile>" -D -n "Caddy Local Authority"     # remove
   ```
   `sql:` selects the SQLite (cert9/key4) DB instead of legacy DBM (<https://wiki.mozilla.org/NSS_Shared_DB>). On Windows
   and macOS the `certutil` binary is the **NSS** one, not the Windows one — it is not shipped with Firefox; macOS needs
   `brew install nss`, Windows needs an NSS build on PATH.
3. **Policy `Certificates.Install`** with a file placed in `%AppData%\Mozilla\Certificates` (Windows) or
   `~/Library/Application Support/Mozilla/Certificates` (macOS); installed with trust string `CT,CT,`.

Caddy itself attempts route 2 opportunistically (`truststore.WithFirefox()`), and on Windows its own source notes
"Certutils is not supported on Windows" — so **on Windows Caddy cannot provision Firefox at all**; the app must use
policy/enterprise-roots instead. <https://github.com/smallstep/truststore/blob/master/truststore_windows.go>

---

## 2. macOS

### 2.1 The two trust domains and the exact commands

`security(1)`: "Add certificate … from certFile to **per-user or local Admin Trust Settings**. When modifying per-user
Trust Settings, user authentication is required via an authentication dialog. When modifying admin Trust Settings, the
process must be running as root, or admin authentication is required." `-d` = "Add to admin cert store; **default is
user**"; `-r` = `trustRoot|trustAsRoot|deny|unspecified`; `-k` = keychain; `-p` = policy constraint
(`ssl`, `basic`, …). <https://keith.github.io/xcode-man-pages/security.1.html>

```bash
# admin domain (root or admin auth) — what Caddy does
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain root.crt

# user domain (no admin) — one GUI authentication panel
security add-trusted-cert -r trustRoot -k "$HOME/Library/Keychains/login.keychain-db" root.crt

security dump-trust-settings -d          # admin-domain settings
security dump-trust-settings             # user-domain settings (default)
```

Apple's own API docs agree, and add two operational constraints worth designing around
(<https://developer.apple.com/documentation/security/sectrustsettingssettrustsettings(_:_:_:)>):

* per-user trust changes "prompt the user with an alert panel asking for authentication … **it is not possible to modify
  per-user trust settings when not running in a GUI environment**";
* admin-domain changes "prompt … for an administrator's name and password unless the calling process is running as root".

Domains: `SecTrustSettingsDomain.user` = "Per-user trust settings"; `.admin` = "Locally administered, system-wide trust
settings. Administrator privileges are required to make changes to this domain."
<https://developer.apple.com/documentation/security/sectrustsettingsdomain/user>,
<https://developer.apple.com/documentation/security/sectrustsettingsdomain/admin>
User overrides admin. `security user-trust-settings-enable -d` *disables* the user domain entirely — an edge case where
per-user trust silently stops working.

### 2.2 What `caddy trust` actually runs on macOS

From smallstep/truststore (the library Caddy calls), darwin backend:

```go
exec.Command("sudo", "security", "add-trusted-cert", "-d", "-k", "/Library/Keychains/System.keychain", filename)
// then trust-settings-export -d / trust-settings-import -d to force explicit
// kSecTrustSettingsPolicy = sslServer|basicX509 with kSecTrustSettingsResult = 1 (trustRoot)
```

and uninstall = `sudo security remove-trusted-cert -d <file>`.
<https://github.com/smallstep/truststore/blob/master/truststore_darwin.go>
So **macOS Caddy = admin domain + sudo + system keychain**, and an unprivileged Caddy process will fail/prompt; the Caddy
docs acknowledge this ("You may need to run this command with `sudo` on unix systems").
<https://caddyserver.com/docs/command-line#caddy-trust>

### 2.3 How Safari and Chrome evaluate the result

* Safari (and everything using `SecTrustEvaluate`): user domain → admin domain → system. Per-user trust therefore
  satisfies Safari.
* Chrome: `net/cert/internal/trust_store_mac.cc` walks the trust domains explicitly —
  `// Evaluate user trust domain, then admin. User settings can override admin` — iterating
  `{kSecTrustSettingsDomainUser, kSecTrustSettingsDomainAdmin}` and consulting
  `SecTrustSettingsCopyTrustSettings`. **Per-user (login-keychain) trust is honoured by Chrome on macOS.**
  <https://chromium.googlesource.com/chromium/src/+/main/net/cert/internal/trust_store_mac.cc>
  The Chrome Root Store FAQ phrases the same rule as "Default and System Keychains … where the 'When using this
  certificate' flag is set to 'Always Trust', or the SSL flag is 'Always Trust'" — which is what `-r trustRoot` without
  `-p` produces (an unconditional trust-root setting).
* Firefox on macOS: same enterprise-roots logic; user domain is processed before admin
  (`kSecTrustSettingsDomainUser`, then `…Admin`, with a `…System` early-exit) in
  <https://github.com/mozilla-firefox/firefox/blob/main/security/manager/ssl/EnterpriseRoots.cpp>.

### 2.4 Does per-user trust cover `*.localhost` and deeper subdomains?

Yes — **trust is bound to the CA root certificate, not to hostnames**. Any leaf Caddy signs for `web.localhost`,
`web.hello.localhost`, `a.b.c.d.localhost` chains to the same trusted root, so the browser verdict is identical for every
depth. (Hostname/name-constraint checks apply to the *leaf's SAN*, which Caddy sets to the exact requested name; a
wildcard leaf only matches one label — see §3.3.) The one caveat is the disabled-user-domain edge case in §2.1.

---

## 3. Caddy specifics

### 3.1 `caddy trust` / `caddy untrust` mechanics

* `caddy trust` **does not read the CA from storage** — it connects to the admin API (default `localhost:2019`),
  `GET /pki/ca/<id>/certificates`, and installs the returned root into local trust store(s). `--ca`, `--address`, `-c` let
  you point elsewhere; when the server runs unprivileged, run `caddy trust` (elevated) yourself.
  <https://caddyserver.com/docs/command-line#caddy-trust>
* `caddy untrust` removes trust for a given root (`--cert`, or fetched from the admin API). It "does not necessarily
  delete the root certificate from trust stores entirely … repeatedly trusting and untrusting new certificates can fill
  up trust databases", and it does not delete Caddy's stored certs.
  <https://caddyserver.com/docs/command-line#caddy-untrust>
* Internally the CA calls `truststore.Install(root, WithDebug(), WithFirefox(), WithJava())` and short-circuits if the
  root is already trusted (`trusted()` probe) to avoid password prompts.
  <https://github.com/caddyserver/caddy/blob/master/modules/caddypki/ca.go>
  Disable with `skip_install_trust` (Caddyfile) / `"install_trust": false` (JSON) if the app manages trust itself —
  <https://caddyserver.com/docs/caddyfile/options#skip-install-trust>

### 3.2 Where the root lives, what it is called, how long it lives

* Storage keys: `pki/authorities/local/root.crt`, `root.key`, `intermediate.crt`, `intermediate.key` (CA id `local`).
* Data directory: Windows `%AppData%\Caddy`; macOS `$HOME/Library/Application Support/Caddy`.
  <https://caddyserver.com/docs/conventions#data-directory>
* Names: CA name `Caddy Local Authority`; root CN `{pki.ca.name} - {time.now.year} ECC Root`; intermediate CN
  `{pki.ca.name} - ECC Intermediate`. That CN is what users see in keychain/certmgr and what you grep for on removal.
* Lifetimes (constants in `ca.go`): `defaultRootLifetime = 24h*30*12*10` → **10 years (3600 d)**;
  `defaultIntermediateLifetime = 7 d`, renewed automatically (maintenance every 10 min, renew inside 20 % of lifetime).
  Caddyfile docs state the root lifetime as fixed: "must be less than the lifetime of the root cert (`3600d` or 10
  years)". <https://caddyserver.com/docs/caddyfile/options#pki-options>
* Only `intermediate_lifetime`, `maintenance_interval`, `renewal_window_ratio`, `name`, `root_cn`, `intermediate_cn`,
  `install_trust`, and explicit `root{}`/`intermediate{}` key pairs are configurable.
* **Cost of changing the root lifetime**: there is no `root_lifetime` knob — you must supply your own root key pair
  (or patch/fork Caddy). A new root means a **new trust anchor on every user machine**: every install must be
  re-approved, the old root stays trusted until removed, and anything pinned to the old root breaks. Changing the
  intermediate lifetime is cheap by contrast (intermediates renew silently and are delivered on the wire).

### 3.3 Does the internal CA issue for `*.localhost` and any depth?

Yes. Caddy/certmagic classify names as internal when
`subj == "localhost" || HasSuffix(subj, ".localhost") || .local || .internal || .home.arpa || internal IP` —
`certmagic.SubjectIsInternal` — and internal names never qualify for public certs
(`SubjectQualifiesForPublicCert`), so they are issued by the internal CA.
<https://github.com/caddyserver/certmagic/blob/master/certificates.go>
The check is a suffix check, so `web.hello.localhost` and deeper (`a.b.c.localhost`) are all "internal" → internal
issuer, no depth limit in the CA.

Two practical caveats:

* A **wildcard site address `*.localhost` yields a wildcard certificate**, and TLS wildcards match exactly one label:
  `web.localhost` yes, `web.hello.localhost` **no**. Either enumerate exact names (Caddy will issue one cert per name,
  all from the internal CA), or use on-demand TLS with an `ask`/`permission` gate, or run a wildcard per depth
  (`*.hello.localhost`). <https://caddyserver.com/docs/automatic-https#wildcard-certificates>
* `*.localhost` resolves to loopback without any DNS configuration (RFC 6761 special-use):
  <https://www.rfc-editor.org/rfc/rfc6761#section-6.3> — no hosts-file edits, and no port-53 listener needed.

---

## 4. Browser reality check

Condition: Caddy internal root installed **per-user** (`CurrentUser\Root` on Windows; user trust domain on macOS).

| Browser | Win 11 per-user | macOS per-user trust | Notes |
|---|---|---|---|
| Chrome | ✅ secure | ✅ secure | CRS verifier reads CurrentUser Root / user+admin trust domains |
| Edge | ✅ secure | ✅ secure | queries platform for locally-installed roots (Edge ≥ 112) |
| Firefox | ❌ default; ✅ with `ImportEnterpriseRoots`/NSS import | ❌ default; ✅ with `ImportEnterpriseRoots` | own NSS DB; enterprise-roots reads HKCU and the macOS user domain |
| Safari | n/a (EOL on Windows) | ✅ secure | system `SecTrustEvaluate` incl. user domain |

UI detail: **there is no green padlock in Chrome any more.** Since Chrome 117 (Chromium blog, May 2023) the padlock was
replaced by the "tune" icon; "secure" is conveyed by the absence of a warning plus the tune icon and
`chrome://certificate-manager` (<https://blog.chromium.org/2023/05/an-update-on-lock-icon.html>,
<https://chromium.googlesource.com/chromium/src/+/main/net/data/ssl/chrome_root_store/faq.md>). Edge follows the same UI.
Firefox and Safari still show a padlock. So the realistic target is **"neutral, no warning"** for Chromium browsers, not
a green lock.

### Where HSTS / localhost special-casing changes the answer

* **HSTS is explicitly ignored for `*.localhost` in Chromium**: in `TransportSecurityState::GetSSLUpgradeDecision`,
  `// [*.]localhost hosts now ignore Strict-Transport-Security response headers, but an entry may have been stored
  before this restriction was introduced (crbug.com/41251622)`, guarded by the `kIgnoreHSTSForLocalhost` feature.
  <https://chromium.googlesource.com/chromium/src/+/main/net/http/transport_security_state.cc>
  Consequence: `Strict-Transport-Security` on a `.localhost` response is a no-op (and cannot brick the app if TLS breaks);
  HSTS preload lists cannot contain `localhost` either.
* **`localhost` is a "potentially trustworthy" origin** per the Secure Contexts spec, so `http://web.hello.localhost` is
  already a secure context: service workers, `crypto.subtle`, WebAuthn, clipboard, `SameSite=None; Secure`-adjacent
  behaviour all work **without TLS**.
  <https://w3c.github.io/webappsec-secure-contexts/#is-origin-trustworthy>
  This is the key asymmetry versus a custom domain, and it is your graceful-degradation story.
* **A custom domain is not special-cased.** `.test`/`.internal`/`.local` names get no default trust, no automatic loopback
  resolution (needs hosts/DNS), HSTS applies normally, and there is no "insecure localhost" escape hatch. If you later
  want `https://app.example.test` to behave like `.localhost`, you need the same CA trust plus DNS work.
* Chromium does **not** waive certificate validation for localhost. The only sanctioned escape hatch is the
  `--allow-insecure-localhost` switch / `chrome://flags` entry ("ignores TLS errors on localhost"), which is unusable for
  a normal user-facing flow.

---

## 5. Least-privilege recommendation

### 5.1 Windows 11 — fully non-elevated

1. Ship Caddy as a user-scoped sidecar; let it generate the CA in `%AppData%\Caddy\pki\authorities\local\`.
2. Keep Caddy's own install-trust behaviour (`install_trust` default `true`) — the smallstep Windows backend opens
   `CertOpenSystemStore(0, "ROOT")`, i.e. **the current user's Root store**, and adds the cert there. No admin, no UAC.
   If you prefer explicit control, set `install_trust false` and run, per user:
   `certutil -addstore -user Root "%AppData%\Caddy\pki\authorities\local\root.crt"` (or the `Import-Certificate`
   equivalent). Verify with `certutil -store -user Root | findstr /i "Caddy"`.
3. **Silent, per-user**: CA generation, leaf issuance/renewal, HKCU trust write, `:443`/`:80` binding, `.localhost`
   resolution (no DNS).
4. **Firefox**: cannot be done silently by an unprivileged app (Firefox has no user-writable policy mechanism that is
   reliable). Offer (a) a one-line instruction to set `security.enterprise_roots.enabled=true` in `about:config` — after
   which your HKCU root is trusted automatically — or (b) a documented `policies.json`/GPO snippet for managed machines,
   or (c) `certutil -d sql:` with an NSS build.
5. **Never write `LocalMachine\Root`** unless the user explicitly asks for "trust for all users on this PC"; that is the
   only step needing elevation.
6. Removal: `certutil -delstore -user Root "Caddy Local Authority"` (or delete by thumbprint), then delete
   `%AppData%\Caddy`. `caddy untrust` also works while the admin API is reachable (it deletes by serial from the same
   user store).

### 5.2 macOS — one prompt, no admin

1. Set `pki { ca local { install_trust false } }` (or `skip_install_trust`) so Caddy never tries `sudo`.
2. After first start, the app runs once, from the GUI session:
   ```bash
   security add-trusted-cert -r trustRoot -k "$HOME/Library/Keychains/login.keychain-db" \
     "$HOME/Library/Application Support/Caddy/pki/authorities/local/root.crt"
   ```
   The user authenticates once in the SecurityAgent panel (password or Touch ID). **No admin credentials, no system
   keychain, no `sudo`.**
3. Do not delegate this to `caddy trust` on macOS: it hardcodes `sudo … -d -k /Library/Keychains/System.keychain`
   (admin domain).
4. **Silent**: everything after the approval — leaf issuance, intermediate renewal every 7 d, HTTP→HTTPS redirects.
5. Firefox on macOS: same `Certificates.ImportEnterpriseRoots` story; the macOS user domain is read first, so the same
   per-user root satisfies Firefox once enterprise roots are enabled.
6. Removal:
   ```bash
   security delete-certificate -c "Caddy Local Authority" -t "$HOME/Library/Keychains/login.keychain-db"
   # -t also drops the user trust settings for that cert (security(1))
   # admin-domain cleanup, if it was ever used:
   sudo security remove-trusted-cert -d root.crt
   sudo security delete-certificate -c "Caddy Local Authority" /Library/Keychains/System.keychain
   ```
   Then delete `~/Library/Application Support/Caddy`. Note `caddy untrust` (macOS) only removes the *admin-domain* entry.

### 5.3 What the user must approve

| Action | Win 11 | macOS |
|---|---|---|
| Generate CA + issues leaf certs | silent | silent |
| Trust the root | silent (HKCU) | **1 authentication panel** (user domain) |
| Trust for all users / all accounts | UAC elevation | admin name+password (`sudo`, `-d`) |
| Firefox trust | user action (`about:config`) or admin policy | same |

### 5.4 What breaks if the user refuses

* Chromium + Safari: `ERR_CERT_AUTHORITY_INVALID` / "This Connection Is Not Private" interstitial on every
  `https://*.localhost` origin; API calls, WebSocket and fetch from the embedded webview all fail (the interstitial is a
  hard stop, not a warning you can click through for XHR).
* Firefox: `SEC_ERROR_UNKNOWN_ISSUER` (even if the OS store is trusted, unless enterprise roots are on).
* Fallback that keeps working: serve the app over **`http://*.localhost`** — a secure context, so service workers,
  `crypto.subtle`, WebAuthn, clipboard and secure-cookie semantics still work (§4). What is lost: TLS-specific behaviour
  (https-only redirects, HSTS, `Secure` cookies over a non-loopback alias), third-party devices on the LAN, and any
  OAuth/IdP redirect URI registered as `https://…`. Show this as an explicit "continue without HTTPS" path rather than
  failing outright.

---

## 6. Source list

* MS Learn — System Store Locations: <https://learn.microsoft.com/en-us/windows/win32/seccrypto/system-store-locations>
* MS Learn — Local machine and current user certificate stores: <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/local-machine-and-current-user-certificate-stores>
* MS Learn — Certificate Stores: <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/certificate-stores>
* MS Learn — `certutil`: <https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/certutil>
* MS Learn — `Import-Certificate`: <https://learn.microsoft.com/en-us/powershell/module/pki/import-certificate>
* MS Learn — `CertOpenSystemStore`: <https://learn.microsoft.com/en-us/windows/win32/api/wincrypt/nf-wincrypt-certopensystemstorea>
* MS Learn — Edge TLS certificate verification change: <https://learn.microsoft.com/en-us/deployedge/microsoft-edge-security-cert-verification>
* Chromium — Chrome Root Store FAQ: <https://chromium.googlesource.com/chromium/src/+/main/net/data/ssl/chrome_root_store/faq.md>
* Chromium — `net/cert/internal/trust_store_mac.cc` (user-then-admin trust domains): <https://chromium.googlesource.com/chromium/src/+/main/net/cert/internal/trust_store_mac.cc>
* Chromium — `net/http/transport_security_state.cc` (HSTS ignored for `*.localhost`, crbug 41251622): <https://chromium.googlesource.com/chromium/src/+/main/net/http/transport_security_state.cc>
* Chromium blog — "An Update on the Lock Icon": <https://blog.chromium.org/2023/05/an-update-on-lock-icon.html>
* Apple — `SecTrustSettingsDomain.user` / `.admin`: <https://developer.apple.com/documentation/security/sectrustsettingsdomain/user>, <https://developer.apple.com/documentation/security/sectrustsettingsdomain/admin>
* Apple — `SecTrustSettingsSetTrustSettings` (auth panel; GUI-session requirement): <https://developer.apple.com/documentation/security/sectrustsettingssettrustsettings(_:_:_:)>
* `security(1)` man page (Xcode mirror; `add-trusted-cert`, `-d`, `-k`, `-r`, `-t` on `delete-certificate`): <https://keith.github.io/xcode-man-pages/security.1.html>
* Mozilla — Firefox administrator reference, `Certificates`: <https://firefox-admin-docs.mozilla.org/reference/policies/certificates/>
* Mozilla — policy templates (`ImportEnterpriseRoots` → `security.enterprise_roots.enabled`): <https://mozilla.github.io/policy-templates/#certificates--importenterpriseroots>
* Mozilla — `EnterpriseRoots.cpp` (HKCU + macOS user domain): <https://github.com/mozilla-firefox/firefox/blob/main/security/manager/ssl/EnterpriseRoots.cpp>
* Mozilla wiki — NSS Shared DB, `sql:` prefix: <https://wiki.mozilla.org/NSS_Shared_DB>
* Caddy — Automatic HTTPS / Local HTTPS: <https://caddyserver.com/docs/automatic-https#local-https>
* Caddy — `caddy trust` / `caddy untrust`: <https://caddyserver.com/docs/command-line#caddy-trust>
* Caddy — global options `pki`, `skip_install_trust`: <https://caddyserver.com/docs/caddyfile/options#pki-options>
* Caddy — data directory conventions: <https://caddyserver.com/docs/conventions#data-directory>
* Caddy — `modules/caddypki/ca.go` (lifetimes, `truststore.Install(..., WithFirefox(), WithJava())`): <https://github.com/caddyserver/caddy/blob/master/modules/caddypki/ca.go>
* smallstep/truststore — darwin backend (`sudo security add-trusted-cert -d -k System.keychain`): <https://github.com/smallstep/truststore/blob/master/truststore_darwin.go>
* smallstep/truststore — windows backend (`CertOpenSystemStore(0,"ROOT")` = per-user): <https://github.com/smallstep/truststore/blob/master/truststore_windows.go>
* certmagic — `SubjectIsInternal` / `SubjectQualifiesForPublicCert`: <https://github.com/caddyserver/certmagic/blob/master/certificates.go>
* W3C — Secure Contexts, "potentially trustworthy origin": <https://w3c.github.io/webappsec-secure-contexts/#is-origin-trustworthy>
* RFC 6761 §6.3 — `.localhost` special-use domain: <https://www.rfc-editor.org/rfc/rfc6761#section-6.3>
