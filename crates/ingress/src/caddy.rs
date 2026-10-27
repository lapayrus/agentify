//! Finding Caddy and running it.
//!
//! The binary is never downloaded: a missing `caddy.exe` is a typed error a
//! user can act on. Lookup order is explicit configuration, then the
//! development tree (`target/tools/`), then a copy cached under the data root by
//! the installer.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::routes::{SERVER_HTTP, SERVER_TLS};

/// Environment variable naming the Caddy binary explicitly.
pub const CADDY_ENV: &str = "AGENTIFY_CADDY";

/// Where the development copy lives, relative to the workspace root.
pub const DEV_CADDY: &str = "target/tools/caddy.exe";

/// Where the installer caches it, relative to the data root.
pub const CACHED_CADDY: &str = "tools/caddy.exe";

/// How long Caddy gets to answer on its admin endpoint.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Where a located binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySource {
    /// `AGENTIFY_CADDY`.
    Configured,
    /// The development tree.
    Development,
    /// The copy cached under the data root.
    Cached,
}

/// A Caddy binary on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaddyBinary {
    pub path: PathBuf,
    pub source: BinarySource,
}

/// Why Caddy could not be found or started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaddyError {
    /// No binary anywhere it could look.
    Missing { searched: Vec<PathBuf> },
    /// The binary exists but could not be started.
    Failed { path: PathBuf, detail: String },
    /// It started but never answered on the admin endpoint.
    NotReady { admin: SocketAddr, detail: String },
}

impl std::fmt::Display for CaddyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaddyError::Missing { searched } => write!(
                f,
                "no caddy executable found (set {CADDY_ENV} to one); looked in: {}",
                searched
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            CaddyError::Failed { path, detail } => {
                write!(f, "starting {} failed: {detail}", path.display())
            }
            CaddyError::NotReady { admin, detail } => {
                write!(f, "caddy never answered on {admin}: {detail}")
            }
        }
    }
}

impl std::error::Error for CaddyError {}

impl CaddyBinary {
    /// Find a binary for this machine, or say where it looked.
    ///
    /// `configured` is `AGENTIFY_CADDY`, resolved by the caller so this function
    /// stays a pure question about the filesystem.
    pub fn locate(data_root: &Path, configured: Option<PathBuf>) -> Result<Self, CaddyError> {
        let mut searched = Vec::new();

        if let Some(path) = configured {
            if path.is_file() {
                return Ok(Self {
                    path,
                    source: BinarySource::Configured,
                });
            }
            // A configured path that does not exist is not a reason to look
            // somewhere else: the user meant that file.
            return Err(CaddyError::Missing {
                searched: vec![path],
            });
        }

        let mut candidates: Vec<(PathBuf, BinarySource)> = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                // `target/debug/agentifyd` and `target/debug/deps/test.exe`
                for up in [1usize, 2usize] {
                    if let Some(root) = ascend(dir, up) {
                        candidates.push((root.join(DEV_CADDY), BinarySource::Development));
                    }
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push((cwd.join(DEV_CADDY), BinarySource::Development));
            // A test binary runs with the crate directory as its cwd.
            if let Some(parent) = cwd.parent().and_then(|parent| parent.parent()) {
                candidates.push((parent.join(DEV_CADDY), BinarySource::Development));
            }
        }
        candidates.push((data_root.join(CACHED_CADDY), BinarySource::Cached));

        for (path, source) in candidates {
            if searched.contains(&path) {
                continue;
            }
            if path.is_file() {
                return Ok(Self { path, source });
            }
            searched.push(path);
        }
        Err(CaddyError::Missing { searched })
    }
}

fn ascend(from: &Path, levels: usize) -> Option<PathBuf> {
    let mut current = from;
    for _ in 0..levels {
        current = current.parent()?;
    }
    Some(current.to_path_buf())
}

/// The config Caddy starts with: two servers, no routes yet, and its own CA.
///
/// * `admin` listens on loopback on a port nobody else knows.
/// * `storage` lives under the data root, so the local CA is Agentify's and its
///   root certificate has a predictable path for the trust installer.
/// * `install_trust: false` — Caddy must never touch a certificate store on its
///   own; installing trust is an explicit, opt-in act.
/// * both servers listen on loopback only, so nothing is reachable from the LAN.
pub fn base_config(
    http_port: u16,
    https_port: u16,
    admin: SocketAddr,
    storage_root: &Path,
) -> Value {
    let storage = storage_root.display().to_string().replace('\\', "/");
    json!({
        "admin": {
            "listen": admin.to_string(),
            // No autosave: a persisted config could resurrect routes the daemon
            // no longer knows about after a restart.
            "config": { "persist": false },
        },
        "storage": {
            "module": "file_system",
            "root": storage,
        },
        "apps": {
            "pki": {
                "certificate_authorities": {
                    "local": {
                        "name": "Agentify Local Authority",
                        "root_common_name": crate::trust::CA_COMMON_NAME,
                        "install_trust": false,
                    }
                }
            },
            "http": {
                "servers": {
                    SERVER_TLS: {
                        "listen": [format!("127.0.0.1:{https_port}")],
                        "routes": [],
                        "automatic_https": { "disable_redirects": true },
                    },
                    SERVER_HTTP: {
                        "listen": [format!("127.0.0.1:{http_port}")],
                        "routes": [],
                        "automatic_https": { "disable": true, "disable_redirects": true },
                    },
                }
            }
        }
    })
}

/// A Caddy process the daemon owns.
#[derive(Debug)]
pub struct CaddyInstance {
    child: Option<Child>,
    admin: SocketAddr,
    binary: PathBuf,
    config_path: PathBuf,
    log_path: PathBuf,
}

impl CaddyInstance {
    /// Ask Caddy whether it would accept this config, so a bad route is a
    /// readable error instead of a process that exits without saying why.
    pub fn validate(binary: &Path, config_path: &Path) -> Result<(), CaddyError> {
        let mut command = Command::new(binary);
        command
            .arg("validate")
            .arg("--config")
            .arg(config_path)
            // The adapter is inferred from the `.json` extension; naming it
            // explicitly is refused by some builds.
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let output = command.output().map_err(|err| CaddyError::Failed {
            path: binary.to_path_buf(),
            detail: err.to_string(),
        })?;
        if output.status.success() {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            detail
        };
        Err(CaddyError::Failed {
            path: binary.to_path_buf(),
            detail: format!("the generated config was refused: {detail}"),
        })
    }

    /// Start Caddy on `config_path` and wait until its admin endpoint answers.
    pub fn spawn(
        binary: &Path,
        config_path: &Path,
        log_path: &Path,
        admin: SocketAddr,
    ) -> Result<Self, CaddyError> {
        Self::validate(binary, config_path)?;
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|err| CaddyError::Failed {
                path: binary.to_path_buf(),
                detail: format!("opening {}: {err}", log_path.display()),
            })?;
        let stderr = log.try_clone().map_err(|err| CaddyError::Failed {
            path: binary.to_path_buf(),
            detail: format!("duplicating the log handle: {err}"),
        })?;

        let mut command = Command::new(binary);
        command
            .arg("run")
            .arg("--config")
            .arg(config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(windows)]
        {
            // No console window for a background sidecar.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let child = command.spawn().map_err(|err| CaddyError::Failed {
            path: binary.to_path_buf(),
            detail: err.to_string(),
        })?;

        let mut instance = Self {
            child: Some(child),
            admin,
            binary: binary.to_path_buf(),
            config_path: config_path.to_path_buf(),
            log_path: log_path.to_path_buf(),
        };
        instance.await_ready()?;
        Ok(instance)
    }

    pub fn admin(&self) -> SocketAddr {
        self.admin
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// True while the process is alive.
    pub fn running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Stop Caddy and wait for it.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn await_ready(&mut self) -> Result<(), CaddyError> {
        let client = crate::http::Http::new(self.admin);
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last = String::from("no response yet");
        while Instant::now() < deadline {
            if !self.running() {
                // Give the fatal line a moment to reach the log before reading.
                std::thread::sleep(Duration::from_millis(250));
                let tail = self.log_tail().unwrap_or_else(|| "no output".to_string());
                return Err(CaddyError::NotReady {
                    admin: self.admin,
                    detail: format!("the process exited: {tail}"),
                });
            }
            match client.get("/config/") {
                Ok(response) if response.is_success() => return Ok(()),
                Ok(response) => last = format!("status {}", response.status),
                Err(err) => last = err.to_string(),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(CaddyError::NotReady {
            admin: self.admin,
            detail: match self.log_tail() {
                Some(tail) => format!("{last}; caddy said: {tail}"),
                None => last,
            },
        })
    }

    /// The end of Caddy's own log: a startup failure has to be readable.
    fn log_tail(&self) -> Option<String> {
        let raw = std::fs::read_to_string(&self.log_path).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let tail: String = trimmed
            .chars()
            .rev()
            .take(1200)
            .collect::<Vec<char>>()
            .into_iter()
            .rev()
            .collect();
        Some(tail)
    }
}

impl Drop for CaddyInstance {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_config_is_loopback_only_and_never_installs_trust() {
        let admin = SocketAddr::from(([127, 0, 0, 1], 41234));
        let config = base_config(18080, 18443, admin, Path::new(r"C:\data\ingress\caddy"));

        assert_eq!(config["admin"]["listen"], "127.0.0.1:41234");
        assert_eq!(
            config["admin"]["config"]["persist"], false,
            "an autosaved config would resurrect routes the daemon does not know about"
        );

        let servers = &config["apps"]["http"]["servers"];
        assert_eq!(servers[SERVER_TLS]["listen"][0], "127.0.0.1:18443");
        assert_eq!(servers[SERVER_HTTP]["listen"][0], "127.0.0.1:18080");
        for server in [SERVER_TLS, SERVER_HTTP] {
            let listen = servers[server]["listen"][0].as_str().expect("listen");
            assert!(
                listen.starts_with("127.0.0.1:") || listen.starts_with("[::1]:"),
                "{server} must not listen on the LAN: {listen}"
            );
        }
        assert_eq!(servers[SERVER_TLS]["routes"], json!([]));
        assert_eq!(servers[SERVER_HTTP]["routes"], json!([]));
    }

    #[test]
    fn the_local_ca_is_ours_and_install_trust_is_off() {
        let config = base_config(
            80,
            443,
            SocketAddr::from(([127, 0, 0, 1], 1234)),
            Path::new("/data/ingress/caddy"),
        );
        let ca = &config["apps"]["pki"]["certificate_authorities"]["local"];
        assert_eq!(ca["root_common_name"], crate::trust::CA_COMMON_NAME);
        assert_eq!(
            ca["install_trust"], false,
            "Caddy must never write to a certificate store on its own"
        );
        assert_eq!(
            config["storage"]["root"], "/data/ingress/caddy",
            "the CA lives under the daemon's data root, so uninstall can find it"
        );
    }

    #[test]
    fn https_does_not_redirect_on_its_own_so_the_daemon_decides() {
        let config = base_config(
            80,
            443,
            SocketAddr::from(([127, 0, 0, 1], 1234)),
            Path::new("/data"),
        );
        let tls = &config["apps"]["http"]["servers"][SERVER_TLS]["automatic_https"];
        assert_eq!(
            tls["disable_redirects"], true,
            "Caddy's own :80 redirect would fight the daemon's HTTP fallback"
        );
        assert!(
            tls.get("disable").is_none() || tls["disable"] == false,
            "the TLS server must still get certificates"
        );
        let http = &config["apps"]["http"]["servers"][SERVER_HTTP]["automatic_https"];
        assert_eq!(http["disable"], true, "the plain server serves no certificates");
    }

    #[test]
    fn a_missing_binary_is_a_typed_error_that_lists_where_it_looked() {
        // A configured path that is not there is the whole answer: the user
        // meant that file, so nothing else is consulted.
        let temp = tempfile::tempdir().expect("temp dir");
        let configured = temp.path().join("not-here.exe");
        let err = CaddyBinary::locate(temp.path(), Some(configured.clone()))
            .expect_err("nothing to find");
        match &err {
            CaddyError::Missing { searched } => {
                assert_eq!(searched, &vec![configured]);
                assert!(err.to_string().contains(CADDY_ENV));
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn a_cached_binary_under_the_data_root_is_found_last() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cached = temp.path().join(CACHED_CADDY);
        std::fs::create_dir_all(cached.parent().expect("parent")).expect("create tools");
        std::fs::write(&cached, b"").expect("write a stand-in binary");

        let located = CaddyBinary::locate(temp.path(), None);
        // In a development build the workspace copy wins; a machine with only
        // the cached copy still works. Either way it is never a download.
        match located {
            Ok(binary) => {
                assert!(binary.path.is_file());
                assert!(matches!(
                    binary.source,
                    BinarySource::Cached | BinarySource::Development
                ));
            }
            Err(err) => panic!("a cached binary must be found: {err}"),
        }
    }
}
