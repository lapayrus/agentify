//! Publishing where the daemon listens.
//!
//! Clients (the desktop shell, the CLI, the MCP shim) find the daemon through
//! one file in the data root. It is written atomically and is readable only by
//! the current OS user.

use std::io::Write;

use agentify_proto::{Discovery, API_VERSION};
use anyhow::{Context, Result};

use crate::acl;
use crate::clock;
use crate::config::Config;

/// Write the discovery file for a freshly started daemon and return its
/// contents.
pub fn write(config: &Config, daemon_version: &str, token: &str, port: u16) -> Result<Discovery> {
    let discovery = Discovery {
        pid: std::process::id(),
        port,
        url: format!("http://127.0.0.1:{port}"),
        token: token.to_string(),
        daemon_version: daemon_version.to_string(),
        api_version: API_VERSION.to_string(),
        created_at: clock::now_rfc3339(),
    };

    let path = config.discovery_path();
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));

    let written = write_atomically(&temp, &path, &discovery);
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written?;
    Ok(discovery)
}

fn write_atomically(temp: &std::path::Path, path: &std::path::Path, discovery: &Discovery) -> Result<()> {
    let body = serde_json::to_vec_pretty(discovery).context("serialising the discovery file")?;
    let mut file = std::fs::File::create(temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    // Tighten the ACL before any content lands on disk.
    acl::restrict_to_current_user(temp)
        .with_context(|| format!("restricting the ACL of {}", temp.display()))?;
    file.write_all(&body)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing {}", temp.display()))?;
    drop(file);
    std::fs::rename(temp, path)
        .with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

/// Remove the discovery file on a clean shutdown, so clients never mistake a
/// dead daemon for a live one.
pub fn remove(config: &Config) {
    let _ = std::fs::remove_file(config.discovery_path());
}
