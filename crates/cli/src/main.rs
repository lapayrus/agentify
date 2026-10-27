//! `agentify` — a thin client of the daemon API.
//!
//! Deliberately small: the walking skeleton needs a way to start the daemon in
//! the foreground, a way to ask a running daemon who it thinks it is, and a way
//! for a human to agree to the one Windows step first run needs.
//! Token issuing, secret reveal, and diagnostics arrive with later tickets.

use std::path::Path;
use std::process::ExitCode;

use agentify_bootstrap::{Bootstrap, BootstrapState, Consent, RESUME_FLAG};
use agentify_proto::client::DaemonClient;
use agentify_proto::lifecycle::{Stop, SystemProcesses};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    match command {
        "daemon" => run_daemon(&args[1..]),
        "whoami" => whoami(),
        "status" => status(),
        "revoke-token" => revoke_token(),
        "bootstrap" => bootstrap(&args[1..]),
        "login-item" => login_item(&args[1..]),
        "path" => path_command(&args[1..]),
        "uninstall" => uninstall(&args[1..]),
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
  agentify daemon stop                stop a running daemon (0 = stopped or absent;
                                      2 = an operation is in flight)
  agentify status                     report whether the daemon is reachable
  agentify whoami                     print the daemon's whoami payload as JSON
  agentify revoke-token               revoke the discovery token (the next daemon
                                      run mints a fresh one)
  agentify bootstrap                  report whether this machine can run Projects
  agentify bootstrap --consent        agree to the one Windows step (a UAC prompt)
  agentify bootstrap --resume         finish setup after the restart
  agentify login-item install         start the daemon at logon (per-user task)
  agentify login-item status          is the login item registered?
  agentify login-item remove          take the login item out again
  agentify path install               put this install directory on the user PATH
  agentify path remove                take it out again
  agentify path status                is it on the user PATH?
  agentify uninstall                  remove login item, trust, and Caddy data
  agentify uninstall --purge          also remove runtimes and the data root

  `agentify bootstrap` on its own only reports: it never installs WSL, enables a
  Windows feature, or asks for an administrator token. Consenting is a human
  action — this flag, or the setup card in the desktop app — and an Agent has no
  way to produce it.

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

/// `agentify bootstrap [status|--consent|--resume]`.
///
/// Exit codes: 0 when the machine is ready, 2 when a human action is still
/// needed (consent, or the restart), 1 when setup failed. A script can tell
/// "nothing to do" from "someone has to say yes" without parsing the output.
fn bootstrap(args: &[String]) -> ExitCode {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(err) => {
            eprintln!("agentify: locating this executable for the resume entry: {err}");
            return ExitCode::FAILURE;
        }
    };
    let bootstrap = Bootstrap::system(
        agentify_proto::resolve_data_root(),
        &program,
        &["bootstrap", RESUME_FLAG],
    );

    let report = match action {
        "status" | "--status" => bootstrap.status(),
        "consent" | "--consent" => bootstrap.consent(Consent::granted_by_human("cli --consent")),
        "resume" | "--resume" => bootstrap.resume(),
        other => {
            eprintln!(
                "agentify: unknown bootstrap action {other:?}; use status, --consent, or --resume"
            );
            return ExitCode::FAILURE;
        }
    };

    println!("bootstrap: {}", report.state.as_str());
    println!("  {}", report.message);
    if let Some(step) = &report.elevated_step {
        println!("  elevated step: {step}");
    }
    if report.reboot_required {
        println!("  a restart is required; Agentify resumes at your next sign-in");
    }

    match report.state {
        BootstrapState::Ready => ExitCode::SUCCESS,
        BootstrapState::Failed => ExitCode::FAILURE,
        // Consent, a restart, or a probe in progress: a human is still needed.
        _ => ExitCode::from(2),
    }
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
            eprintln!(
                "agentify: unknown login-item action {other:?}; use install, status, or remove"
            );
            ExitCode::FAILURE
        }
    }
}

/// `agentify path install|remove|status`.
///
/// The installer puts its program directory on the user PATH so an MCP client
/// can be configured with `agentify-mcp` instead of an absolute path. Removal
/// takes out exactly the entry the installer added.
fn path_command(args: &[String]) -> ExitCode {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let program_dir = match std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        Some(directory) => directory,
        None => {
            eprintln!("agentify: could not locate this executable's directory");
            return ExitCode::FAILURE;
        }
    };

    match action {
        "install" => match agentify_daemon::path_entry::install(&program_dir) {
            Ok(true) => {
                println!("path: added {}", program_dir.display());
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!(
                    "path: {} is already on the user PATH",
                    program_dir.display()
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agentify: {err:#}");
                ExitCode::FAILURE
            }
        },
        "remove" => match agentify_daemon::path_entry::remove(&program_dir) {
            Ok(true) => {
                println!("path: removed {}", program_dir.display());
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!(
                    "path: {} was not on the user PATH",
                    program_dir.display()
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agentify: {err:#}");
                ExitCode::FAILURE
            }
        },
        "status" => {
            if agentify_daemon::path_entry::contains(&program_dir) {
                println!("path: {} is on the user PATH", program_dir.display());
                ExitCode::SUCCESS
            } else {
                println!("path: {} is not on the user PATH", program_dir.display());
                ExitCode::FAILURE
            }
        }
        other => {
            eprintln!("agentify: unknown path action {other:?}; use install, remove, or status");
            ExitCode::FAILURE
        }
    }
}

/// `agentify uninstall [--purge]`.
///
/// Non-destructive by default: it removes the login item, the trusted root, and
/// Caddy data, and leaves Projects and the state database. `--purge` is the
/// explicit, confirmed removal that also deletes runtimes and the data root.
fn uninstall(args: &[String]) -> ExitCode {
    let purge = args.iter().any(|arg| arg == "--purge");
    let data_root = agentify_proto::resolve_data_root();

    // A running daemon holds state.db and its Caddy; stop it first so those files
    // can be removed. Absence is not an error: there may be nothing to stop.
    if let Err(err) = stop_daemon(&data_root) {
        eprintln!("agentify: {err:#}");
        return ExitCode::FAILURE;
    }

    match agentify_daemon::uninstall::run_system(&data_root, purge) {
        Ok(report) => {
            if purge {
                println!("uninstall: purge complete");
            } else {
                println!("uninstall: complete; Projects and state are left in place");
            }
            if report.removed_login_item {
                println!("  login item removed");
            }
            if report.removed_trust {
                println!("  trusted root removed");
            }
            if report.removed_caddy {
                println!("  Caddy data removed");
            }
            for distro in &report.removed_runtimes {
                println!("  removed runtime {distro}");
            }
            if report.removed_data_root {
                println!("  data root removed");
            } else {
                println!("  data root kept at {}", data_root.display());
            }
            for note in &report.notes {
                println!("  note: {note}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("agentify: uninstall failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Ask a running daemon to stop, whatever build it is.
///
/// A missing or stale discovery file is "nothing to stop", never a failure: an
/// uninstall must finish after a crash. An Operation in flight is the one
/// refusal — swapping or deleting files underneath it would lose work.
fn stop_daemon(data_root: &std::path::Path) -> anyhow::Result<()> {
    match agentify_proto::lifecycle::stop(data_root, &SystemProcesses) {
        Ok(Stop::Busy { pid, found }) => anyhow::bail!(
            "the daemon (pid {pid}, {found}) has an Operation in flight; wait for it to finish, then uninstall again"
        ),
        Ok(_) => Ok(()),
        Err(err) => Err(anyhow::anyhow!("{err}")),
    }
}

/// `agentify daemon stop`.
///
/// Exit codes: 0 when nothing was running or it stopped, 2 when an Operation is
/// in flight, 1 on a real failure. The installer uses 2 to refuse an
/// install-over rather than terminate work in progress.
fn stop_daemon_command() -> ExitCode {
    let data_root = agentify_proto::resolve_data_root();
    match agentify_proto::lifecycle::stop(&data_root, &SystemProcesses) {
        Ok(Stop::NotRunning) => {
            println!("daemon: not running");
            ExitCode::SUCCESS
        }
        Ok(Stop::Stopped { pid, found }) => {
            println!(
                "daemon: stopped pid {pid} ({})",
                found.unwrap_or_else(|| "unknown build".to_string())
            );
            ExitCode::SUCCESS
        }
        Ok(Stop::Busy { pid, found }) => {
            eprintln!(
                "agentify: the daemon (pid {pid}, {found}) has an Operation in flight; wait for it to finish, then retry"
            );
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!("agentify: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_daemon(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("stop") => return stop_daemon_command(),
        Some(other) => {
            eprintln!(
                "agentify: unknown daemon action {other:?}; run `agentify daemon`, or `agentify daemon stop`"
            );
            return ExitCode::FAILURE;
        }
        None => {}
    }
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
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("agentify: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `agentify revoke-token`: revoke the token in the discovery file.
fn revoke_token() -> ExitCode {
    match DaemonClient::from_env().revoke_token() {
        Ok(payload) => {
            println!(
                "token revoked: {}",
                payload["revoked"].as_bool().unwrap_or(false)
            );
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
