//! `agentify-mcp` — MCP stdio shim for the Agentify daemon.

use std::process::ExitCode;

use agentify_mcp::{serve, DaemonClient};

fn main() -> ExitCode {
    let client = DaemonClient::from_env();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    match serve(stdin.lock(), stdout.lock(), &client) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("agentify-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}
