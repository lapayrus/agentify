//! `agentifyd` — the Agentify daemon.
//!
//! Long-lived, per-user, loopback-only. Prints its data root and listening
//! address to stderr; clients find it through the discovery file instead.
//!
//! A release build has no console: it is started by the shell or the MCP shim
//! and logs to `logs/daemon.log` under the data root.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

use agentify_daemon::Config;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("agentifyd: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            // No configuration means no data root from it; the default root is
            // still where a log belongs.
            agentify_daemon::log_error(&agentify_proto::resolve_data_root(), &format!("{err:#}"));
            return Err(err);
        }
    };
    let data_root = config.data_root.clone();

    let running = match agentify_daemon::start_in_background(config) {
        Ok(running) => running,
        Err(err) => {
            agentify_daemon::log_error(&data_root, &format!("{err:#}"));
            return Err(err);
        }
    };
    eprintln!(
        "agentifyd ready on {} (api {})",
        running.base_url(),
        agentify_proto::API_VERSION
    );

    running.wait()
}
