//! The Tauri shell's only logic: ask the daemon who it is.
//!
//! The window is a client like the CLI and the MCP shim. It reads the discovery
//! file (through `agentify_proto`) and calls `GET /v1/whoami`; it caches nothing
//! and decides nothing about Projects, Environments, or Resources.

use std::path::Path;

use serde::Serialize;

/// What the shell shows in its header and main pane.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonStatus {
    /// The daemon answered.
    Connected {
        whoami: serde_json::Value,
        data_root: String,
    },
    /// The daemon is not running, or its discovery file is unusable.
    Unreachable { reason: String, data_root: String },
}

/// Resolve the daemon's state for a data root.
pub fn status_for(data_root: &Path) -> DaemonStatus {
    let client = agentify_proto::client::DaemonClient::new(data_root);
    match client.whoami() {
        Ok(whoami) => DaemonStatus::Connected {
            whoami,
            data_root: data_root.display().to_string(),
        },
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
