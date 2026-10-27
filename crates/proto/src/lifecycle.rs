//! Reconcile a caller with the daemon it finds in a data root.
//!
//! A discovery file is a hint, not proof. Its pid may be gone, an unrelated
//! process may have reused it, or a daemon from another build may still be
//! serving the port. A caller trusts it only when the process is alive, the
//! liveness route answers, and the daemon reports this build's version. Anything
//! else is a *stray daemon*: it is stopped — gracefully first, and by pid only
//! when it has stopped answering — before the caller starts its own.
//!
//! Every OS effect is behind [`ProcessTable`], so a test never kills a real
//! process. The HTTP side is the same client every other caller uses.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::{ClientError, DaemonClient};
use crate::{Discovery, API_VERSION};

/// How long a graceful stop may take before termination is attempted.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long a freshly started daemon has to publish a discovery file and answer.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

const POLL: Duration = Duration::from_millis(50);
const TERMINATE_GRACE: Duration = Duration::from_secs(2);
const DAEMON_IMAGE: &str = "agentifyd";

/// What `GET /v1/healthz` says about the daemon that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub daemon_version: String,
    pub api_version: String,
}

impl Probe {
    /// Read the two facts the trust decision needs out of the liveness payload.
    ///
    /// The liveness route is unauthenticated and no token is read, so a caller
    /// can probe a data root before it decides to use it.
    pub fn from_healthz(value: &Value) -> Option<Self> {
        Some(Self {
            daemon_version: value.get("daemon_version")?.as_str()?.to_string(),
            api_version: value.get("api_version")?.as_str()?.to_string(),
        })
    }
}

/// What a discovery file amounts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Alive, answering, and the build the caller ships.
    Trusted,
    /// Nothing is there: no file, or a file naming a process that is gone.
    Absent,
    /// A live process that does not answer its own liveness route.
    Unresponsive { pid: u32 },
    /// A live daemon of another build.
    Foreign { pid: u32, found: String },
}

/// The trust decision, with no I/O.
pub fn classify(
    pid: u32,
    process_alive: bool,
    probe: Option<&Probe>,
    expected_version: &str,
) -> Verdict {
    if !process_alive {
        return Verdict::Absent;
    }
    let Some(probe) = probe else {
        return Verdict::Unresponsive { pid };
    };
    if probe.daemon_version != expected_version || probe.api_version != API_VERSION {
        return Verdict::Foreign {
            pid,
            found: label(probe),
        };
    }
    Verdict::Trusted
}

/// The OS-side process questions the reconciliation needs.
pub trait ProcessTable {
    /// The executable image name for `pid`, or `None` when the process is gone.
    fn image(&self, pid: u32) -> Option<String>;
    /// Terminate `pid` and its children, forcefully.
    fn terminate(&self, pid: u32) -> Result<(), String>;
}

/// What `ensure` found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensure {
    /// A daemon of this build was already running and was left alone.
    Running(Discovery),
    /// A stray was dealt with (or nothing was running) and the shipped daemon
    /// was started.
    Started(Discovery),
}

/// Why `ensure` could not leave a usable daemon running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureError {
    /// A daemon is in the middle of an Operation, so it was not replaced.
    Busy { pid: u32, found: String },
    /// The stray could not be stopped.
    StopFailed(String),
    /// The caller's spawn step failed.
    SpawnFailed(String),
    /// The spawned daemon never published a usable discovery file.
    StartFailed(String),
}

impl std::fmt::Display for EnsureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnsureError::Busy { pid, found } => write!(
                f,
                "the running daemon (pid {pid}) is build {found} and has an Operation in flight; \
                 wait for it to finish, then try again"
            ),
            EnsureError::StopFailed(detail) => {
                write!(f, "stopping the stray daemon failed: {detail}")
            }
            EnsureError::SpawnFailed(detail) => write!(f, "starting the daemon failed: {detail}"),
            EnsureError::StartFailed(detail) => {
                write!(f, "the daemon did not become usable: {detail}")
            }
        }
    }
}

impl std::error::Error for EnsureError {}

/// What `stop` found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// No daemon was running; a stale discovery file was removed if there was one.
    NotRunning,
    /// The daemon was asked to stop (or terminated) and is gone.
    Stopped { pid: u32, found: Option<String> },
    /// A daemon is running an Operation and must not be stopped.
    Busy { pid: u32, found: String },
}

/// Find (and if needed replace) the daemon for a data root.
///
/// `spawn` is called at most once, only when nothing usable is running. It must
/// start the daemon the caller ships; this function waits for that daemon to
/// publish a discovery file reporting `expected_version`.
pub fn ensure(
    data_root: &Path,
    expected_version: &str,
    processes: &dyn ProcessTable,
    spawn: impl FnOnce() -> Result<(), String>,
) -> Result<Ensure, EnsureError> {
    match reconcile(data_root, expected_version, processes)? {
        Some(discovery) => Ok(Ensure::Running(discovery)),
        None => {
            spawn().map_err(EnsureError::SpawnFailed)?;
            let discovery = wait_for_daemon(data_root, expected_version, processes)
                .map_err(EnsureError::StartFailed)?;
            Ok(Ensure::Started(discovery))
        }
    }
}

/// Stop whatever daemon a data root names, whatever build it is.
///
/// A missing or stale discovery file is "nothing to stop", never an error: an
/// uninstall must be able to finish after a crash. An in-flight Operation is the
/// one refusal — a caller that deletes files or swaps binaries must wait.
pub fn stop(data_root: &Path, processes: &dyn ProcessTable) -> Result<Stop, String> {
    let Some(discovery) = read_discovery_or_clean(data_root) else {
        return Ok(Stop::NotRunning);
    };

    if processes.image(discovery.pid).is_none() {
        remove_discovery(data_root);
        return Ok(Stop::NotRunning);
    }

    let client = DaemonClient::new(data_root);
    let Some(probe) = probe(data_root) else {
        // It does not answer, so there is nothing to drain. Terminate only a
        // process whose image really is the daemon: a reused pid is not ours.
        let Some(image) = processes.image(discovery.pid) else {
            remove_discovery(data_root);
            return Ok(Stop::NotRunning);
        };
        if !is_daemon_image(&image) {
            remove_discovery(data_root);
            return Ok(Stop::NotRunning);
        }
        processes.terminate(discovery.pid).map_err(|err| {
            format!(
                "the daemon (pid {}) stopped answering and could not be terminated: {err}",
                discovery.pid
            )
        })?;
        let gone = wait_gone(data_root, discovery.pid, processes, TERMINATE_GRACE);
        remove_discovery(data_root);
        return if gone {
            Ok(Stop::Stopped {
                pid: discovery.pid,
                found: None,
            })
        } else {
            Err(format!("the daemon (pid {}) did not stop", discovery.pid))
        };
    };

    let found = label(&probe);
    match gate(&client) {
        Gate::InFlight | Gate::Unknown => {
            return Ok(Stop::Busy {
                pid: discovery.pid,
                found,
            })
        }
        Gate::Stale => {
            // The file's token does not belong to the daemon answering on that
            // port: the file is a leftover, not a claim about a live process.
            remove_discovery(data_root);
            return Ok(Stop::NotRunning);
        }
        Gate::Clear => {}
    }

    // Graceful first: the daemon removes its own discovery file on the way out.
    let _ = client.shutdown();
    if wait_gone(data_root, discovery.pid, processes, STOP_GRACE) {
        return Ok(Stop::Stopped {
            pid: discovery.pid,
            found: Some(found),
        });
    }

    // It ignored the request. Terminate by pid, but only our own image.
    if let Some(image) = processes.image(discovery.pid) {
        if is_daemon_image(&image) {
            let _ = processes.terminate(discovery.pid);
        }
    }
    let gone = wait_gone(data_root, discovery.pid, processes, TERMINATE_GRACE);
    remove_discovery(data_root);
    if gone {
        Ok(Stop::Stopped {
            pid: discovery.pid,
            found: Some(found),
        })
    } else {
        Err(format!("the daemon (pid {}) did not stop", discovery.pid))
    }
}

/// Is this image name the Agentify daemon? Paths and case do not matter.
pub fn is_daemon_image(image: &str) -> bool {
    let name = image
        .rsplit(|character| character == '\\' || character == '/')
        .next()
        .unwrap_or(image);
    name.eq_ignore_ascii_case(DAEMON_IMAGE) || name.eq_ignore_ascii_case("agentifyd.exe")
}

/// `Some(discovery)` when a daemon of this build is already usable, `None` when
/// the caller should start one.
fn reconcile(
    data_root: &Path,
    expected_version: &str,
    processes: &dyn ProcessTable,
) -> Result<Option<Discovery>, EnsureError> {
    let Some(discovery) = read_discovery_or_clean(data_root) else {
        return Ok(None);
    };
    let alive = processes.image(discovery.pid).is_some();
    let probe = if alive { probe(data_root) } else { None };

    match classify(discovery.pid, alive, probe.as_ref(), expected_version) {
        Verdict::Trusted => Ok(Some(discovery)),
        Verdict::Absent => {
            remove_discovery(data_root);
            Ok(None)
        }
        Verdict::Unresponsive { .. } | Verdict::Foreign { .. } => {
            match stop(data_root, processes).map_err(EnsureError::StopFailed)? {
                Stop::Busy { pid, found } => Err(EnsureError::Busy { pid, found }),
                _ => Ok(None),
            }
        }
    }
}

/// How the daemon's own update gate answers.
enum Gate {
    /// No Operation is in flight.
    Clear,
    /// An Operation is in flight.
    InFlight,
    /// The gate could not be read, so stopping is not safe.
    Unknown,
    /// The discovery file's token is not this daemon's: the file is stale.
    Stale,
}

fn gate(client: &DaemonClient) -> Gate {
    match client.update_readiness() {
        Ok(value) => match value.get("operations_in_flight").and_then(Value::as_bool) {
            Some(true) => Gate::InFlight,
            Some(false) => Gate::Clear,
            None => Gate::Unknown,
        },
        Err(ClientError::Status { status, .. }) if status == 401 || status == 403 => Gate::Stale,
        Err(_) => Gate::Unknown,
    }
}

fn probe(data_root: &Path) -> Option<Probe> {
    DaemonClient::new(data_root)
        .healthz()
        .ok()
        .and_then(|value| Probe::from_healthz(&value))
}

fn read_discovery_or_clean(data_root: &Path) -> Option<Discovery> {
    match crate::read_discovery(data_root) {
        Ok(discovery) => Some(discovery),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            // An unreadable file is no better than no file.
            remove_discovery(data_root);
            None
        }
    }
}

fn remove_discovery(data_root: &Path) {
    let _ = std::fs::remove_file(crate::discovery_path(data_root));
}

/// Wait until the discovery file no longer names `pid`, or the process is gone.
fn wait_gone(data_root: &Path, pid: u32, processes: &dyn ProcessTable, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let still_named = crate::read_discovery(data_root)
            .map(|discovery| discovery.pid == pid)
            .unwrap_or(false);
        if !still_named || processes.image(pid).is_none() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Wait for the just-started daemon to publish a discovery file this build
/// trusts.
fn wait_for_daemon(
    data_root: &Path,
    expected_version: &str,
    processes: &dyn ProcessTable,
) -> Result<Discovery, String> {
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    loop {
        if let Ok(discovery) = crate::read_discovery(data_root) {
            if processes.image(discovery.pid).is_some() {
                if let Some(probe) = probe(data_root) {
                    if probe.daemon_version == expected_version && probe.api_version == API_VERSION
                    {
                        return Ok(discovery);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no daemon reported build {expected_version} within {}s",
                SPAWN_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(POLL);
    }
}

fn label(probe: &Probe) -> String {
    if probe.api_version == API_VERSION {
        probe.daemon_version.clone()
    } else {
        format!("{} (api {})", probe.daemon_version, probe.api_version)
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use super::ProcessTable;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The real machine: `tasklist` for liveness and `taskkill` to terminate.
    pub struct SystemProcesses;

    impl ProcessTable for SystemProcesses {
        fn image(&self, pid: u32) -> Option<String> {
            let filter = format!("PID eq {pid}");
            let output = Command::new("tasklist.exe")
                .args(["/FI", &filter, "/FO", "CSV", "/NH"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()?;
            parse_tasklist(&String::from_utf8_lossy(&output.stdout), pid)
        }

        fn terminate(&self, pid: u32) -> Result<(), String> {
            let output = Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|err| format!("running taskkill.exe: {err}"))?;
            if output.status.success() || self.image(pid).is_none() {
                return Ok(());
            }
            let detail = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Err(format!("taskkill for pid {pid} failed: {}", detail.trim()))
        }
    }

    /// Parse `tasklist /FO CSV /NH` output, keeping only the row for `pid`.
    ///
    /// A row is `"image","pid","session","number","memory"`. Anything else —
    /// including a localized "no tasks" line — is no match.
    pub(super) fn parse_tasklist(output: &str, pid: u32) -> Option<String> {
        for line in output.lines() {
            let line = line.trim();
            if !line.starts_with('"') {
                continue;
            }
            let mut fields = line.split("\",\"");
            let image = fields.next()?.trim_start_matches('"').to_string();
            let found = fields.next()?.trim_matches('"');
            if found.parse::<u32>().ok() == Some(pid) {
                return Some(image);
            }
        }
        None
    }
}

#[cfg(unix)]
mod platform {
    use std::process::Command;

    use super::ProcessTable;

    /// The Unix equivalent of `tasklist` and `taskkill`. v0 does not ship a
    /// Unix daemon; these exist so the crate builds and a caller is not
    /// silently lied to.
    pub struct SystemProcesses;

    impl ProcessTable for SystemProcesses {
        fn image(&self, pid: u32) -> Option<String> {
            let output = Command::new("ps")
                .args(["-o", "comm=", "-p", &pid.to_string()])
                .output()
                .ok()?;
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!name.is_empty()).then_some(name)
        }

        fn terminate(&self, pid: u32) -> Result<(), String> {
            let status = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status()
                .map_err(|err| format!("running kill: {err}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("kill -9 {pid} failed"))
            }
        }
    }
}

pub use platform::SystemProcesses;

#[cfg(all(test, windows))]
mod tasklist_tests {
    use super::platform::parse_tasklist;

    #[test]
    fn the_row_for_the_pid_wins() {
        let output = "\"notepad.exe\",\"111\",\"Console\",\"1\",\"10,000 K\"\r\n\
                      \"agentifyd.exe\",\"6496\",\"Console\",\"1\",\"8,300 K\"\r\n";
        assert_eq!(
            parse_tasklist(output, 6496).as_deref(),
            Some("agentifyd.exe")
        );
        assert_eq!(parse_tasklist(output, 999), None);
    }

    #[test]
    fn a_localized_no_match_line_is_not_an_image() {
        assert_eq!(
            parse_tasklist("INFO: No tasks are running.\r\n", 6496),
            None
        );
        assert_eq!(parse_tasklist("", 6496), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(version: &str, api: &str) -> Probe {
        Probe {
            daemon_version: version.to_string(),
            api_version: api.to_string(),
        }
    }

    #[test]
    fn a_live_daemon_of_this_build_is_trusted() {
        let probe = probe(crate::versions::DAEMON_VERSION, API_VERSION);
        assert_eq!(
            classify(1, true, Some(&probe), crate::versions::DAEMON_VERSION),
            Verdict::Trusted
        );
    }

    #[test]
    fn a_dead_pid_is_absent_even_with_a_matching_version_in_the_file() {
        assert_eq!(classify(1, false, None, "0.1.2"), Verdict::Absent);
    }

    #[test]
    fn a_live_process_that_does_not_answer_is_unresponsive() {
        assert_eq!(
            classify(41, true, None, "0.1.2"),
            Verdict::Unresponsive { pid: 41 }
        );
    }

    #[test]
    fn another_build_is_foreign_and_named() {
        let probe = probe("0.1.1", API_VERSION);
        assert_eq!(
            classify(42, true, Some(&probe), "0.1.2"),
            Verdict::Foreign {
                pid: 42,
                found: "0.1.1".to_string()
            }
        );
    }

    #[test]
    fn an_api_change_is_foreign_even_when_the_version_matches() {
        let probe = probe("0.1.2", "v2");
        assert_eq!(
            classify(43, true, Some(&probe), "0.1.2"),
            Verdict::Foreign {
                pid: 43,
                found: "0.1.2 (api v2)".to_string()
            }
        );
    }

    #[test]
    fn the_daemon_image_is_recognised_through_a_path_and_any_case() {
        assert!(is_daemon_image("agentifyd.exe"));
        assert!(is_daemon_image("AgentifyD.EXE"));
        assert!(is_daemon_image(
            "C:\\Users\\someone\\AppData\\Local\\Programs\\Agentify\\agentifyd.exe"
        ));
        assert!(is_daemon_image("/usr/local/bin/agentifyd"));
        assert!(!is_daemon_image("agentify-mcp.exe"));
        assert!(!is_daemon_image("agentify.exe"));
    }

    #[test]
    fn the_healthz_payload_carries_the_two_facts() {
        let value = serde_json::json!({
            "status": "ok",
            "daemon_version": "0.1.2",
            "api_version": "v1"
        });
        assert_eq!(Probe::from_healthz(&value), Some(probe("0.1.2", "v1")));
        assert_eq!(
            Probe::from_healthz(&serde_json::json!({ "status": "ok" })),
            None
        );
    }
}
