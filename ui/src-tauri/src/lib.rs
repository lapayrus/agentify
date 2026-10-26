//! The Tauri shell's only logic: ask the daemon who it is, what Projects it has,
//! and how the selected Environment is doing.
//!
//! The window is a client like the CLI and the MCP shim. It reads the discovery
//! file (through `agentify_proto`) and calls the loopback API; it caches nothing
//! and decides nothing about Projects, Environments, or Resources.

use std::path::Path;

use serde::Serialize;

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
                data_root: data_root.display().to_string(),
            }
        }
        Err(err) => DaemonStatus::Unreachable {
            reason: err.to_string(),
            data_root: data_root.display().to_string(),
        },
    }
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

#[tauri::command]
fn daemon_status() -> DaemonStatus {
    status_for(&data_root())
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

/// Start the shell.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            daemon_status,
            environment_status,
            environment_stats,
            environment_logs,
            environment_action
        ])
        .run(tauri::generate_context!())
        .expect("the Agentify shell failed to start");
}
