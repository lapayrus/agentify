//! Agentify MCP stdio shim.
//!
//! MCP clients (Claude Desktop, Codex, anything speaking JSON-RPC over stdio)
//! launch this binary. It holds no state of its own: it reads the daemon's
//! discovery file and proxies tool calls to the loopback API, so an Agent sees
//! exactly what the human UI sees.

pub mod rpc;

use std::io::{self, BufRead, Write};

pub use agentify_proto::client::{ClientError, DaemonClient};
pub use rpc::PROTOCOL_VERSION;

/// Serve JSON-RPC over the given streams until the input closes.
pub fn serve<R: BufRead, W: Write>(input: R, mut output: W, client: &DaemonClient) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = rpc::handle_line(&line, client) {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
