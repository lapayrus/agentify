//! `agentify` — a thin client of the daemon API.
//!
//! Deliberately small: the walking skeleton needs a way to start the daemon in
//! the foreground and a way to ask a running daemon who it thinks it is.
//! Token issuing, secret reveal, and diagnostics arrive with later tickets.

use std::process::ExitCode;

use agentify_proto::client::DaemonClient;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    match command {
        "daemon" => run_daemon(),
        "whoami" => whoami(),
        "status" => status(),
        "help" | "--help" | "-h" => {
            usage();
            ExitCode::SUCCESS
        }
        "--version" | "-V" => {
            println!("agentify {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("agentify: unknown command {other:?}");
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    println!(
        "agentify {} — local, agent-native environments

Usage:
  agentify daemon    run the daemon in the foreground
  agentify status    report whether the daemon is reachable
  agentify whoami    print the daemon's whoami payload as JSON

Environment:
  {}   override the data root (default: per-user local app data)
  AGENTIFY_HOME is where the state database and discovery file live.",
        env!("CARGO_PKG_VERSION"),
        agentify_proto::HOME_ENV
    );
}

fn run_daemon() -> ExitCode {
    match agentify_daemon::Config::from_env().and_then(agentify_daemon::start_in_background) {
        Ok(running) => {
            println!(
                "agentifyd {} listening on {} (api {})",
                agentify_daemon::DAEMON_VERSION,
                running.base_url(),
                agentify_proto::API_VERSION
            );
            match running.wait() {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("agentify: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(err) => {
            eprintln!("agentify: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn whoami() -> ExitCode {
    let client = DaemonClient::from_env();
    match client.whoami() {
        Ok(payload) => {
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("agentify: {err}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    let client = DaemonClient::from_env();
    match (client.discovery(), client.whoami()) {
        (Ok(discovery), Ok(payload)) => {
            println!(
                "daemon connected at {} (daemon {}, api {}, data root {})",
                discovery.url,
                payload["daemon_version"].as_str().unwrap_or("?"),
                payload["api_version"].as_str().unwrap_or("?"),
                client.data_root().display()
            );
            ExitCode::SUCCESS
        }
        (discovery, whoami) => {
            let reason = whoami.err().or_else(|| discovery.err().map(|err| err));
            match reason {
                Some(err) => eprintln!("agentify: {err}"),
                None => eprintln!("agentify: the daemon is not reachable"),
            }
            ExitCode::FAILURE
        }
    }
}
