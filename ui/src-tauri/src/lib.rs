//! The Tauri shell's only logic: ask the daemon who it is and what Projects it
//! has.
//!
//! The window is a client like the CLI and the MCP shim. It reads the discovery
//! file (through `agentify_proto`) and calls `GET /v1/whoami` and
//! `GET /v1/projects`; it caches nothing and decides nothing about Projects,
//! Environments, or Resources.

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

/// Command the front end calls to paint a frame.
#[tauri::command]
fn daemon_status() -> DaemonStatus {
    status_for(&agentify_proto::resolve_data_root())
}

/// Start the shell.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![daemon_status])
        .run(tauri::generate_context!())
        .expect("the Agentify shell failed to start");
}
