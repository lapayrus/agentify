//! The version handshake, taken once at startup.
//!
//! The app pins API v1, the daemon and MCP versions (shipped together), the
//! Caddy version, each Backend adapter's major, and the rootfs and image
//! digests. This module collects what is actually here — without starting
//! anything — and compares it against the pin table in
//! [`agentify_proto::versions`].
//!
//! A mismatch does not stop the daemon: it refuses that **Backend**, and the
//! report is served at `GET /v1/handshake` and in `/v1/healthz`, so the shell can
//! say which component is wrong instead of an Agent discovering it through a
//! failing `start`.
//!
//! Only the `wsl` adapter sees the rootfs cache; the fake has no substrate, so
//! its rootfs row is reported as not checked.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use agentify_backends::{Backend, BackendError};
use agentify_proto::versions::{self, Observed, VersionReport};
use agentify_proto::DataRoot;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// What the daemon found, and its verdict.
#[derive(Debug, Clone)]
pub struct Handshake {
    pub report: VersionReport,
    /// Set when a component mismatched: the sentence the API repeats.
    pub refusal: Option<String>,
}

impl Handshake {
    /// True when the Backend may be driven.
    pub fn backend_allowed(&self) -> bool {
        self.refusal.is_none()
    }
}

/// Take the handshake for this configuration and this Backend.
///
/// `backend` is the adapter the daemon just built — building one is cheap and
/// side-effect free, so the check can describe what it would drive without
/// driving it.
pub fn check(config: &Config, backend: &Arc<dyn Backend>) -> Handshake {
    let observed = observe(config, backend);
    let report = versions::evaluate(&observed);
    let refusal = if report.ok {
        None
    } else {
        Some(format!(
            "the {} Backend is refused: {}. Reinstall Agentify so the app, the \
             daemon, the MCP shim, Caddy, and the Backend adapters come from one \
             build.",
            observed.adapter.as_deref().unwrap_or("selected"),
            report.summary()
        ))
    };
    Handshake { report, refusal }
}

/// Collect what is present on this machine.
pub fn observe(config: &Config, backend: &Arc<dyn Backend>) -> Observed {
    let info = backend.describe();
    let data_root = DataRoot::new(&config.data_root);
    Observed {
        api: Some(agentify_proto::API_VERSION.to_string()),
        daemon: Some(versions::DAEMON_VERSION.to_string()),
        mcp: Some(versions::MCP_VERSION.to_string()),
        caddy: caddy_version(config),
        adapter: Some(info.name.clone()),
        adapter_version: Some(info.version.clone()),
        rootfs_sha256: rootfs_digest(&data_root, &info.name),
        images: Vec::new(),
    }
}

/// The Caddy binary's version, when one is installed and ingress is wanted.
///
/// A missing binary is not a handshake failure: ingress reports its own typed
/// `Missing` error when it is started, and the shell can install a build that
/// has one. A binary that is present but the wrong version is a failure.
fn caddy_version(config: &Config) -> Option<String> {
    config.ingress.as_ref()?;
    let configured = std::env::var_os(agentify_ingress::caddy::CADDY_ENV)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from);
    let binary = agentify_ingress::CaddyBinary::locate(&config.data_root, configured).ok()?;
    run_version(&binary.path)
}

/// `<caddy> version` → `2.10.2`.
fn run_version(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = if stdout.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        stdout.to_string()
    };
    versions::parse_caddy_version(&line)
}

/// SHA-256 of the cached base rootfs, when one has been downloaded.
///
/// The adapter verifies the digest again before it imports anything; this read
/// exists so the handshake can report the digest it would use.
fn rootfs_digest(data_root: &DataRoot, adapter: &str) -> Option<String> {
    if adapter != "wsl" {
        return None;
    }
    let cached = data_root
        .rootfs_cache()
        .join(agentify_backends::wsl::rootfs::PINNED_ROOTFS_FILE);
    if !cached.is_file() {
        return None;
    }
    sha256_file(&cached).ok()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The typed error a refused Backend returns, as a `LifecycleError`.
pub fn refusal_error(refusal: &str) -> BackendError {
    BackendError::VersionMismatch {
        expected: "the versions this build pins".to_string(),
        found: refusal.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentify_backends::{FakeBackend, FakeScript};

    fn config() -> Config {
        Config::new(std::env::temp_dir().join("agentify-handshake-test"))
    }

    fn shipped_fake() -> Arc<dyn Backend> {
        Arc::new(FakeBackend::new(FakeScript::default()))
    }

    #[test]
    fn the_shipped_fake_passes_the_handshake() {
        let handshake = check(&config(), &shipped_fake());
        assert!(
            handshake.backend_allowed(),
            "the shipped fake must match its own pins: {:?}",
            handshake.report
        );
        assert_eq!(handshake.report.components[0].component, "api");
    }

    #[test]
    fn an_adapter_on_another_major_is_refused() {
        let script = FakeScript {
            version: Some("9.0.0".to_string()),
            ..Default::default()
        };
        let handshake = check(&config(), &(Arc::new(FakeBackend::new(script)) as Arc<dyn Backend>));
        assert!(!handshake.backend_allowed());
        let refusal = handshake.refusal.expect("a refusal sentence");
        assert!(refusal.contains("adapter:fake"), "{refusal}");
        assert!(refusal.contains("9.0.0"), "{refusal}");
    }

    #[test]
    fn a_rootfs_that_is_not_there_is_not_checked() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut config = config();
        config.data_root = temp.path().to_path_buf();
        let observed = observe(&config, &shipped_fake());
        // The fake has no substrate, so it never reports a rootfs digest.
        assert_eq!(observed.rootfs_sha256, None);
        assert_eq!(observed.adapter.as_deref(), Some("fake"));
    }

    #[test]
    fn a_caddy_version_line_parses() {
        assert_eq!(
            versions::parse_caddy_version("v2.10.2 h1:abcdef\n"),
            Some("2.10.2".to_string())
        );
    }
}
