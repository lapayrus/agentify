//! Agentify MCP stdio shim.
//!
//! MCP clients (Claude Desktop, Codex, anything speaking JSON-RPC over stdio)
//! launch this binary. It holds no state of its own: it reads the daemon's
//! discovery file and proxies tool calls to the loopback API, so an Agent sees
//! exactly what the human UI sees.

pub mod rpc;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub use agentify_proto::client::{ClientError, DaemonClient};
pub use rpc::PROTOCOL_VERSION;

/// The stable code a shim sees when no daemon answered at all.
const NO_DAEMON_CODE: &str = "BackendUnavailable";

/// Serve JSON-RPC over the given streams until the input closes.
///
/// The shim is what makes an Agent's use of Agentify work with the app closed:
/// a call that finds no daemon starts one beside this binary and is retried
/// once. Nothing was mutated by an attempt that never connected, so the retry
/// is safe.
pub fn serve<R: BufRead, W: Write>(input: R, mut output: W, client: &DaemonClient) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut response = rpc::handle_line(&line, client);
        if response.as_ref().is_some_and(daemon_refused) && ensure_daemon(client) {
            response = rpc::handle_line(&line, client);
        }
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Make sure a daemon this build ships is running.
///
/// `lifecycle::ensure` reuses a live one, replaces one left by another build,
/// and starts one when the data root has none. Best-effort: a failure leaves the
/// original "no daemon" error for the caller to see.
pub fn ensure_daemon(client: &DaemonClient) -> bool {
    agentify_proto::lifecycle::ensure(
        client.data_root(),
        env!("CARGO_PKG_VERSION"),
        &agentify_proto::lifecycle::SystemProcesses,
        spawn_daemon,
    )
    .is_ok()
}

/// Start the daemon beside this shim.
///
/// Its stdio is detached on purpose: the shim owns its own for JSON-RPC, and a
/// line from the daemon on stdout would corrupt the protocol.
fn spawn_daemon() -> Result<(), String> {
    let exe = daemon_binary()?;
    let mut command = Command::new(&exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("starting {} failed: {err}", exe.display()))
}

fn daemon_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("locating the shim executable: {err}"))?;
    let directory = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let name = if cfg!(windows) {
        "agentifyd.exe"
    } else {
        "agentifyd"
    };
    let path = directory.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "no {name} beside {}; install the app or stage the sidecars",
            exe.display()
        ))
    }
}

/// Did the daemon refuse because there was no daemon?
fn daemon_refused(response: &serde_json::Value) -> bool {
    response
        .get("error")
        .and_then(|error| error.get("data"))
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        == Some(NO_DAEMON_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_daemon_triggers_a_restart() {
        assert!(daemon_refused(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32603, "message": "no daemon", "data": { "code": "BackendUnavailable" } }
        })));
        assert!(!daemon_refused(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32602, "message": "bad arguments", "data": { "code": "ValidationFailed" } }
        })));
        assert!(!daemon_refused(&serde_json::json!({ "result": {} })));
    }
}
