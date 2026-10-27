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
        "login-item" => login_item(&args[1..]),
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
  agentify daemon                     run the daemon in the foreground
  agentify status                     report whether the daemon is reachable
  agentify whoami                     print the daemon's whoami payload as JSON
  agentify login-item install         start the daemon at logon (per-user task)
  agentify login-item status          is the login item registered?
  agentify login-item remove          take the login item out again

  Every login-item subcommand takes --name <task name>; the default is the
  name the installer registers. Nothing asks for elevation: the task runs as
  the current user.

Environment:
  {}   override the data root (default: per-user local app data),
       where the state database, logs, Checkpoints, runtime disks, and the
       discovery file live.",
        env!("CARGO_PKG_VERSION"),
        agentify_proto::HOME_ENV
    );
}

/// `agentify login-item install|status|remove`.
fn login_item(args: &[String]) -> ExitCode {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let name = args
        .iter()
        .position(|arg| arg == "--name")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| agentify_daemon::login_item::DEFAULT_TASK_NAME.to_string());

    let item = match agentify_daemon::login_item::LoginItem::for_daemon(name) {
        Ok(item) => item,
        Err(err) => {
            eprintln!("agentify: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    match action {
        "install" => match item.install() {
            Ok(()) => {
                println!(
                    "login item {:?} installed: {} at logon, restarting on failure",
                    item.name,
                    item.program.display()
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agentify: {err:#}");
                ExitCode::FAILURE
            }
        },
        "remove" => match item.remove() {
            Ok(()) => {
                println!("login item {:?} removed", item.name);
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agentify: {err:#}");
                ExitCode::FAILURE
            }
        },
        "status" => match item.status() {
            Ok(status) if status.registered => {
                println!(
                    "login item {:?} is registered: {}",
                    status.name,
                    status.detail.unwrap_or_default()
                );
                ExitCode::SUCCESS
            }
            Ok(status) => {
                println!("login item {:?} is not registered", status.name);
                ExitCode::FAILURE
            }
            Err(err) => {
                eprintln!("agentify: {err:#}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("agentify: unknown login-item action {other:?}; use install, status, or remove");
            ExitCode::FAILURE
        }
    }
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
