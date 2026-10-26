//! `agentifyd` — the Agentify daemon.
//!
//! Long-lived, per-user, loopback-only. Prints its data root and listening
//! address to stderr; clients find it through the discovery file instead.

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
    let config = Config::from_env()?;
    eprintln!(
        "agentifyd {} starting with data root {}",
        agentify_daemon::DAEMON_VERSION,
        config.data_root.display()
    );

    let running = agentify_daemon::start_in_background(config)?;
    eprintln!(
        "agentifyd ready on {} (api {})",
        running.base_url(),
        agentify_proto::API_VERSION
    );

    running.wait()
}
