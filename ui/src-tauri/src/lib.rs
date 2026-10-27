//! The Tauri shell's only logic: ask the daemon who it is, what Projects it has,
//! how the selected Environment is doing, which Approvals are waiting, and what
//! has happened.
//!
//! The window is a client like the CLI and the MCP shim. It reads the discovery
//! file (through `agentify_proto`) and calls the loopback API; it caches nothing
//! and decides nothing about Projects, Environments, or Resources.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri_plugin_updater::UpdaterExt;

/// What the shell shows in its header, rail, and main pane.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonStatus {
    /// The daemon answered.
    Connected {
        whoami: serde_json::Value,
        /// The Projects as `GET /v1/projects` serves them; the shell renders
        /// them and keeps no copy.
        projects: Vec<serde_json::Value>,
        /// Set when the daemon answered but listing Projects failed, so the
        /// shell does not claim there are none.
        #[serde(rename = "projectsError", skip_serializing_if = "Option::is_none")]
        projects_error: Option<String>,
        /// The startup handshake: pinned versions, what was found, and the
        /// refusal when an adapter is on another major. `null` when the daemon
        /// could not be asked, which is not the same as "everything matches".
        handshake: Option<serde_json::Value>,
        data_root: String,
    },
    /// The daemon is not running, or its discovery file is unusable.
    Unreachable { reason: String, data_root: String },
}

/// Resolve the daemon's state for a data root.
pub fn status_for(data_root: &Path) -> DaemonStatus {
    let client = agentify_proto::client::DaemonClient::new(data_root);
    match client.whoami() {
        Ok(whoami) => {
            let (projects, projects_error) = match client.list_projects() {
                Ok(payload) => (
                    payload
                        .get("projects")
                        .and_then(|projects| projects.as_array())
                        .cloned()
                        .unwrap_or_default(),
                    None,
                ),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            DaemonStatus::Connected {
                whoami,
                projects,
                projects_error,
                handshake: handshake_for(data_root),
                data_root: data_root.display().to_string(),
            }
        }
        Err(err) => DaemonStatus::Unreachable {
            reason: err.to_string(),
            data_root: data_root.display().to_string(),
        },
    }
}

/// The daemon's version handshake, or `None` when it cannot be read.
///
/// A handshake that cannot be read is not a handshake that passed: the shell
/// shows the absence rather than claiming the versions match.
pub fn handshake_for(data_root: &Path) -> Option<serde_json::Value> {
    agentify_proto::client::DaemonClient::new(data_root)
        .handshake()
        .ok()
}

/// The update gate: is a mutation in flight, and has every desired-running
/// Environment been adopted and found healthy?
pub fn readiness_for(data_root: &Path) -> Result<serde_json::Value, String> {
    agentify_proto::client::DaemonClient::new(data_root)
        .update_readiness()
        .map_err(|err| err.to_string())
}

/// Ask the daemon to stop, so an update can swap its files or an uninstall can
/// delete them.
pub fn shutdown_for(data_root: &Path) -> Result<serde_json::Value, String> {
    agentify_proto::client::DaemonClient::new(data_root)
        .shutdown()
        .map_err(|err| err.to_string())
}

/// The Environment's status snapshot, as the daemon serves it.
pub fn environment_status_for(data_root: &Path, project: &str) -> Result<serde_json::Value, String> {
    agentify_proto::client::DaemonClient::new(data_root)
        .environment_status(project, agentify_proto::DEV_ENVIRONMENT)
        .map_err(|err| err.to_string())
}

/// The Environment's CPU, memory, and disk figures.
pub fn environment_stats_for(data_root: &Path, project: &str) -> Result<serde_json::Value, String> {
    agentify_proto::client::DaemonClient::new(data_root)
        .stats(project, agentify_proto::DEV_ENVIRONMENT)
        .map_err(|err| err.to_string())
}

/// A page of logs. `since` continues from a previous response's `nextSince`.
pub fn environment_logs_for(
    data_root: &Path,
    project: &str,
    tail: Option<usize>,
    since: Option<&str>,
) -> Result<serde_json::Value, String> {
    agentify_proto::client::DaemonClient::new(data_root)
        .logs(
            project,
            agentify_proto::DEV_ENVIRONMENT,
            None,
            tail,
            since,
        )
        .map_err(|err| err.to_string())
}

/// What a Start or Stop click produced.
#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    /// The action that ran: `start` or `stop`.
    pub action: String,
    /// True when the daemon completed it rather than refusing.
    pub ok: bool,
    /// The status snapshot on success.
    pub status: Option<serde_json::Value>,
    /// The daemon's stable problem code on failure.
    pub code: Option<String>,
    /// A human-readable explanation on failure.
    pub detail: Option<String>,
    /// The operation to wait on when the daemon timed out.
    pub operation_id: Option<String>,
}

/// Start or stop an Environment. The daemon owns the decision; the shell only
/// reports what came back.
pub fn environment_action_for(data_root: &Path, project: &str, action: &str) -> ActionResult {
    let client = agentify_proto::client::DaemonClient::new(data_root);
    let environment = agentify_proto::DEV_ENVIRONMENT;
    let result = match action {
        "start" => client.start_environment(project, environment),
        "stop" => client.stop_environment(project, environment),
        other => {
            return ActionResult {
                action: other.to_string(),
                ok: false,
                status: None,
                code: Some("ValidationFailed".to_string()),
                detail: Some(format!("unknown action {other:?}; start or stop an Environment")),
                operation_id: None,
            }
        }
    };
    match result {
        Ok(status) => ActionResult {
            action: action.to_string(),
            ok: true,
            status: Some(status),
            code: None,
            detail: None,
            operation_id: None,
        },
        Err(err) => {
            let problem = err.problem();
            let field = |name: &str| {
                problem
                    .as_ref()
                    .and_then(|problem| problem.get(name))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            };
            ActionResult {
                action: action.to_string(),
                ok: false,
                status: None,
                code: Some(err.code()),
                detail: Some(field("detail").unwrap_or_else(|| err.to_string())),
                operation_id: field("operationId"),
            }
        }
    }
}

/// Set `AGENTIFY_HOME` for the shell's own client calls.
fn data_root() -> std::path::PathBuf {
    agentify_proto::resolve_data_root()
}

// ---- first run: does this machine have WSL 2? ------------------------------
//
// Three commands, and the important thing is which of them can raise a UAC
// prompt. `bootstrap_status` and `bootstrap_resume` only read: they probe the
// machine, and the resume path can remove a `RunOnce` entry the app itself
// wrote. Only `bootstrap_consent` runs the one elevated step, and the window
// calls it from a click on the setup card — nothing else in the shell can reach
// it. That is the whole consent mechanism on this side: a button, not an agent.

/// The first-run state of this machine, read-only.
pub fn bootstrap_status_for(data_root: &Path, program: &Path) -> agentify_bootstrap::Report {
    bootstrap(data_root, program).status()
}

/// The human's agreement to the one administrator step.
pub fn bootstrap_consent_for(data_root: &Path, program: &Path) -> agentify_bootstrap::Report {
    bootstrap(data_root, program).consent(agentify_bootstrap::Consent::granted_by_human("ui"))
}

/// Finish a setup that was interrupted by a restart. Never elevates.
pub fn bootstrap_resume_for(data_root: &Path, program: &Path) -> agentify_bootstrap::Report {
    bootstrap(data_root, program).resume()
}

fn bootstrap(data_root: &Path, program: &Path) -> agentify_bootstrap::Bootstrap {
    // The resume entry relaunches *this* app with the flag, so Windows finishes
    // the install at the next sign-in without a second prompt.
    agentify_bootstrap::Bootstrap::system(
        data_root,
        program,
        &[agentify_bootstrap::RESUME_FLAG],
    )
}

/// The shell executable, which is what a `RunOnce` entry has to relaunch.
fn program() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("Agentify.exe"))
}

// ---- the Approval inbox and the activity feed ------------------------------

/// The Approvals the daemon holds. `state` is `pending`, `approved`, `denied`,
/// `expired`, or `all`; the daemon defaults to `pending`.
pub fn approvals_for(
    data_root: &Path,
    state: Option<&str>,
    limit: Option<usize>,
) -> Result<serde_json::Value, String> {
    let client = agentify_proto::client::DaemonClient::new(data_root);
    let mut payload = client
        .approvals(state)
        .map_err(|err| err.to_string())?;
    if let Some(limit) = limit {
        if let Some(approvals) = payload
            .get_mut("approvals")
            .and_then(|approvals| approvals.as_array_mut())
        {
            approvals.truncate(limit);
        }
    }
    Ok(payload)
}

/// The audit chain as the activity feed serves it: newest first, with its
/// verdict.
pub fn activity_for(
    data_root: &Path,
    limit: Option<usize>,
) -> Result<serde_json::Value, String> {
    agentify_proto::client::DaemonClient::new(data_root)
        .activity(limit)
        .map_err(|err| err.to_string())
}

/// What an Approve or Deny produced.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionResult {
    /// True when the daemon recorded the decision.
    pub ok: bool,
    /// The decided Approval, as `GET /v1/approvals` serves it.
    pub approval: Option<serde_json::Value>,
    /// The operation the decision released or ended.
    pub operation: Option<serde_json::Value>,
    /// The daemon's stable problem code on failure.
    pub code: Option<String>,
    /// A human-readable explanation on failure.
    pub detail: Option<String>,
}

/// Approve or deny one Approval. The daemon is the gate: approving prompts for
/// the OS gesture, and the shell cannot produce it on its own. `operation` and
/// `params` are what the card showed, so a yes covers exactly that.
pub fn decide_approval_for(
    data_root: &Path,
    approval: &str,
    approve: bool,
    operation: Option<&str>,
    params: Option<&str>,
) -> DecisionResult {
    let client = agentify_proto::client::DaemonClient::new(data_root);
    let mut body = serde_json::Map::new();
    if let Some(operation) = operation {
        body.insert(
            "operationId".to_string(),
            serde_json::Value::String(operation.to_string()),
        );
    }
    if let Some(params) = params {
        body.insert(
            "paramsHash".to_string(),
            serde_json::Value::String(params.to_string()),
        );
    }
    let body = serde_json::Value::Object(body);
    let result = if approve {
        client.approve(approval, &body)
    } else {
        client.deny(approval, &body)
    };
    match result {
        Ok(payload) => DecisionResult {
            ok: true,
            approval: payload.get("approval").cloned(),
            operation: payload.get("operation").cloned(),
            code: None,
            detail: None,
        },
        Err(err) => {
            let problem = err.problem();
            let field = |name: &str| {
                problem
                    .as_ref()
                    .and_then(|problem| problem.get(name))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            };
            DecisionResult {
                ok: false,
                approval: None,
                operation: None,
                code: Some(err.code()),
                detail: Some(field("detail").unwrap_or_else(|| err.to_string())),
            }
        }
    }
}

#[tauri::command]
fn daemon_status() -> DaemonStatus {
    status_for(&data_root())
}

/// The first-run state, for the setup card. A read: no elevation.
#[tauri::command]
fn bootstrap_status() -> agentify_bootstrap::Report {
    bootstrap_status_for(&data_root(), &program())
}

/// The consent action: called from a click on the setup card, and from nowhere
/// else. It is the one path in the shell that can raise a UAC prompt.
#[tauri::command]
fn bootstrap_consent() -> agentify_bootstrap::Report {
    bootstrap_consent_for(&data_root(), &program())
}

/// Finish a restart-interrupted setup. Never elevates.
#[tauri::command]
fn bootstrap_resume() -> agentify_bootstrap::Report {
    bootstrap_resume_for(&data_root(), &program())
}

/// The daemon's version handshake on demand, for a "check again" control.
#[tauri::command]
fn handshake() -> Result<serde_json::Value, String> {
    handshake_for(&data_root()).ok_or_else(|| {
        format!(
            "the daemon did not answer at {}; no handshake to report",
            data_root().display()
        )
    })
}

#[tauri::command]
fn approvals(state: Option<String>, limit: Option<usize>) -> Result<serde_json::Value, String> {
    approvals_for(&data_root(), state.as_deref(), limit)
}

#[tauri::command]
fn activity(limit: Option<usize>) -> Result<serde_json::Value, String> {
    activity_for(&data_root(), limit)
}

#[tauri::command]
fn decide_approval(
    approval: String,
    approve: bool,
    operation: Option<String>,
    params: Option<String>,
) -> DecisionResult {
    decide_approval_for(
        &data_root(),
        &approval,
        approve,
        operation.as_deref(),
        params.as_deref(),
    )
}

#[tauri::command]
fn environment_status(project: String) -> Result<serde_json::Value, String> {
    environment_status_for(&data_root(), &project)
}

#[tauri::command]
fn environment_stats(project: String) -> Result<serde_json::Value, String> {
    environment_stats_for(&data_root(), &project)
}

#[tauri::command]
fn environment_logs(
    project: String,
    tail: Option<usize>,
    since: Option<String>,
) -> Result<serde_json::Value, String> {
    environment_logs_for(&data_root(), &project, tail, since.as_deref())
}

#[tauri::command]
fn environment_action(project: String, action: String) -> ActionResult {
    environment_action_for(&data_root(), &project, &action)
}

// ---- updates and uninstall -------------------------------------------------
//
// The shell is a thin client here too: it asks the daemon whether an update is
// safe, asks it to stop before files are swapped or deleted, and lets the Tauri
// updater plugin move the bits. The daemon's startup adoption is what brings
// running Environments back after the new daemon starts.

/// The daemon's update gate, for a "check for updates" that refuses politely.
#[tauri::command]
fn update_readiness() -> Result<serde_json::Value, String> {
    readiness_for(&data_root())
}

/// Stop the daemon so its sidecar can be replaced or removed.
#[tauri::command]
fn shutdown_daemon() -> Result<serde_json::Value, String> {
    shutdown_for(&data_root())
}

/// A sidecar the installer stages beside this shell. The installer puts every
/// binary in one directory, so the shell can always find its own daemon and MCP
/// shim.
fn sidecar_binary(name: &str) -> PathBuf {
    let exe =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agentify-shell.exe"));
    let dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    })
}

/// The daemon sidecar, beside this shell.
fn daemon_binary() -> PathBuf {
    sidecar_binary("agentifyd")
}

/// The MCP stdio shim, beside this shell.
fn mcp_binary() -> PathBuf {
    sidecar_binary("agentify-mcp")
}

/// Stop the old daemon and start the one beside this shell, so an updated build
/// takes over and adopts the runtimes it left running.
#[tauri::command]
fn restart_daemon() -> Result<(), String> {
    let _ = shutdown_for(&data_root());
    std::process::Command::new(daemon_binary())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("starting the daemon failed: {err}"))
}

/// The tools the MCP shim advertises, in the order the README documents them.
const MCP_TOOLS: &[&str] = &[
    "whoami",
    "create_project",
    "list_projects",
    "get_project",
    "start_environment",
    "stop_environment",
    "get_status",
    "run_command",
    "read_logs",
    "get_stats",
    "list_secrets",
    "create_secret",
    "update_secret",
    "delete_secret",
    "wait_for_operation",
];

/// How to point an MCP client at the shim beside this shell.
#[derive(Debug, Clone, Serialize)]
struct McpInfo {
    /// Absolute path of `agentify-mcp`.
    command: String,
    /// True when the shim is actually installed beside the shell.
    available: bool,
    /// A copy-paste client configuration for the stdio transport.
    config: String,
    /// Every tool the shim advertises.
    tools: Vec<&'static str>,
}

/// The shell's own facts for the Settings and MCP tabs: the build it is, where
/// its data lives, and how to register the MCP connector.
#[derive(Debug, Clone, Serialize)]
struct ShellInfo {
    app_version: String,
    data_root: String,
    discovery_file: String,
    mcp: McpInfo,
}

/// What this shell is, where it keeps its data, and how to register the MCP
/// shim. A read: it neither starts nor stops anything.
#[tauri::command]
fn shell_info() -> ShellInfo {
    let root = data_root();
    let root_text = root.display().to_string();
    let command = mcp_binary();
    let command_text = command.display().to_string();
    // Carry the data root explicitly: an MCP client is often launched by the
    // desktop rather than by this shell, so it would otherwise resolve the
    // default root instead of the one this app is using.
    let config = serde_json::json!({
        "mcpServers": {
            "agentify": {
                "command": command_text.clone(),
                "env": { "AGENTIFY_HOME": root_text.clone() }
            }
        }
    });
    ShellInfo {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        data_root: root_text,
        discovery_file: agentify_proto::discovery_path(&root).display().to_string(),
        mcp: McpInfo {
            available: command.exists(),
            command: command_text,
            config: serde_json::to_string_pretty(&config).unwrap_or_default(),
            tools: MCP_TOOLS.to_vec(),
        },
    }
}

/// The update the updater knows about.
#[derive(Debug, Clone, Serialize)]
struct UpdateInfo {
    version: String,
    body: Option<String>,
}

/// The pending update, if any, and the version this build is.
#[derive(Debug, Clone, Serialize)]
struct UpdateCheck {
    current_version: String,
    update: Option<UpdateInfo>,
}

/// Ask the Tauri updater what is available. The current version is always
/// reported, so the shell can show it even when the endpoint cannot be reached.
#[tauri::command]
async fn update_check(app: tauri::AppHandle) -> Result<UpdateCheck, String> {
    let current_version = app.package_info().version.to_string();
    let updater = app.updater().map_err(|err| err.to_string())?;
    let update = updater.check().await.map_err(|err| err.to_string())?;
    Ok(UpdateCheck {
        current_version,
        update: update.map(|update| UpdateInfo {
            version: update.version.clone(),
            body: update.body.clone(),
        }),
    })
}

/// Download and install the available update, refusing only while a mutation is
/// in flight. The install restarts the shell; `restart_daemon` is what brings the
/// daemon up on the new build when the updater does not restart it itself.
#[tauri::command]
async fn update_install(app: tauri::AppHandle) -> Result<(), String> {
    // A daemon that is not running has no operations, so an unreachable daemon
    // must not block the update. Only a daemon that answers can refuse it.
    if let Ok(readiness) = readiness_for(&data_root()) {
        if readiness
            .get("operations_in_flight")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return Err(
                "an operation is in flight; try the update again when it finishes".to_string(),
            );
        }
    }

    // Stop the sidecar before the installer runs: a live `agentifyd.exe` holds
    // the old file and would make the replacement fail. The restarted shell
    // starts the new daemon from the install directory.
    let _ = shutdown_for(&data_root());

    let updater = app.updater().map_err(|err| err.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "no update is available".to_string())?;
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|err| err.to_string())
}

/// Start the daemon beside this shell when it is not already running.
///
/// The login item starts it at logon, so a shell that opens later finds it
/// already there. This covers the first launch right after install, before the
/// next logon, and any launch where the daemon has been stopped.
fn ensure_daemon() {
    let discovery = agentify_proto::discovery_path(&data_root());
    if discovery.exists() {
        return;
    }
    let _ = std::process::Command::new(daemon_binary()).spawn();
    // Give the daemon a moment to publish its discovery file before the UI's
    // first status read, so a first launch does not flash the unreachable card.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if discovery.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Start the shell.
pub fn run() {
    ensure_daemon();

    // Windows relaunches the app from the per-user `RunOnce` entry after the
    // restart that enabling WSL requires. That is the resume: it reads the
    // machine and finishes setup, and it never elevates.
    if std::env::args().any(|arg| arg == agentify_bootstrap::RESUME_FLAG) {
        let report = bootstrap_resume_for(&data_root(), &program());
        eprintln!(
            "agentify: bootstrap resume: {} — {}",
            report.state.as_str(),
            report.message
        );
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            daemon_status,
            bootstrap_status,
            bootstrap_consent,
            bootstrap_resume,
            handshake,
            environment_status,
            environment_stats,
            environment_logs,
            environment_action,
            approvals,
            activity,
            decide_approval,
            update_readiness,
            shell_info,
            shutdown_daemon,
            restart_daemon,
            update_check,
            update_install
        ])
        .run(tauri::generate_context!())
        .expect("the Agentify shell failed to start");
}
