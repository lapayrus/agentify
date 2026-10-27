# The daemon's lifetime follows the app

The daemon is app-scoped, not machine-scoped. The shell starts it on demand and stops it when the last Agentify
process exits; the MCP shim starts one for its own session when none answers. There is no login item, no
Scheduled Task, and no Windows service, so nothing from Agentify is resident while the app is closed. The daemon
runs without a console window, and the shell holds it in a Windows Job Object with `KILL_ON_JOB_CLOSE`, so the
Caddy child and anything else it spawned die with it.

The alternatives were to keep the per-user logon task and only hide its console, and to keep the logon task but
add a Quit that stops everything. Both leave a daemon the user did not start and cannot see, which is what made
the v0.1.2 install feel like it had parts the app did not own: a terminal window at logon, a process that
survived closing the app, and a discovery file that outlived the process it named. A service was never an
option: it would run as SYSTEM, outside the user's trust boundary, with the wrong profile and the wrong
`%LOCALAPPDATA%`.

Consequences: an MCP client used with the UI closed pays a cold start on its first call, and the shim must be
able to start a daemon at all. Ownership is handed over by adoption: the shell takes over a live daemon it did
not start and stops it on exit, so the shim starts a new one on demand when that daemon goes away. A daemon
nobody owns exits after ten idle minutes, but never while an Environment is running or an Operation is in
flight. A release daemon has no console, so its diagnostics must be a log file under the data root.
The login-item code path, the installer's `POSTINSTALL`/`PREUNINSTALL` calls to `agentify login-item`, and the
`agentify login-item` CLI survive only long enough to unregister tasks left by v0.1.2. This supersedes the
"starts at login … works with the UI closed" criterion in
`.scratch/agentify-v0/issues/11-installable-app.md`, and the README's login-item paragraphs, from the release
that ships it.
