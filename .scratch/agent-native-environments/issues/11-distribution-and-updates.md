# Define distribution, bootstrap, and updates

Type: grilling
Status: resolved
Blocked by: 08, 09, 13

## Question

How does the app install, bootstrap, and update?

Decide Windows packaging (MSIX versus installer) and the one-time elevation for WSL, macOS notarization and background-item registration, and the size cost of bundled rootfs and VM images.

Decide runtime version pinning, the auto-update channel, crash reporting and telemetry opt-in, and what happens to running Projects during an update. State the first-run experience on a machine with no virtualization enabled.

Uninstall must remove the trusted root and the Caddy data directory, using the removal commands in ticket 14.

## Answer

**Windows**: per-user NSIS installer, Authenticode-signed, no admin. It ships the app, the daemon sidecar, Caddy, and the helpers. First run detects WSL without elevation (`wsl --version`, optional-feature state, `HypervisorPresent`; never gate on SLAT), then offers an explicit-consent UAC `wsl --install --no-distribution` with a RunOnce resume after reboot. It then downloads the pinned Ubuntu base rootfs, starts the daemon and loopback Caddy, and installs the per-user root with Firefox enterprise roots.

**macOS**: notarized DMG, Developer ID, hardened runtime, Apple silicon only; Intel is refused with a clear message. Apple container on macOS 26+, Lima on 14–15. `SMAppService` login item. One user-domain trust prompt. The `com.apple.vm.networking` entitlement is filed early; loopback publishing is used until it is granted.

**Linux**: excluded from v0.

Runtime acquisition: bundle Caddy, Lima, and the Apple container CLI/helpers (Apache-2.0/MIT). Download the Ubuntu base rootfs and guest images at first use with pinned digests. Never redistribute Docker Desktop, OrbStack, or Store WSL payloads.

Data: one non-roaming root — `%LOCALAPPDATA%\Agentify\` on Windows, `~/Library/Application Support/Agentify/` on macOS — holding the state database, logs, Checkpoints, rootfs cache, runtime disks, and a user-only discovery file.

Daemon lifecycle: per-user login item with auto-restart. Never a Windows service, which would run as SYSTEM and break per-user tokens and WSL. The UI and MCP shim are clients.

Version pinning: the app pins API v1, the daemon and MCP versions shipped together, the Caddy version, backend adapter majors (`container`, `limactl`), and rootfs and image digests. A mismatch yields `VersionMismatch` and that backend is refused with an explanation.

Updates: signed, stable by default with an opt-in beta. Background download, applied on quit or restart, refused while an operation is in flight. One previous version is kept; the new daemon must pass a health check and adopt existing runtimes or the app rolls back automatically. Running runtimes are never stopped by an update; Projects that fail adoption are marked degraded. Substrate images update separately from the app behind a compatibility check.

Telemetry: opt-in crash reports only, local logs with bounded retention, no analytics.

Uninstall: the Windows uninstaller and the macOS removal path delete the app, daemon sidecar, login item, Caddy data, and the trusted root, but leave Projects. Removing runtimes, volumes, Checkpoints, and the state database is a separate in-app action with explicit confirmation, using ticket 14's removal commands.
