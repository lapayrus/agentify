//! `exec` inside a Project's distro: streamed, cancellable, no PTY.
//!
//! The shape is forced by the platform, the same way the supervisor is. One
//! `wsl.exe -d <distro> -- <command>` is a *session*: it stays alive while the
//! command runs, its two pipes are the command's own stdout and stderr, and its
//! exit status is the command's exit status. So an exec is one launcher, two
//! reader threads (which is what keeps the streams apart instead of merged), and
//! a waiter that publishes the exit code once both pipes have reached end of
//! file.
//!
//! Cancellation is the interesting half. A host-side `kill` of `wsl.exe` would
//! leave the guest process running, so the adapter installed
//! [`EXEC_PATH`] in the distro: it puts the command in its own process group and
//! records the group id, and [`cancel_script`] terminates that group — and then
//! sweeps every process carrying the exec's unique marker, which catches a child
//! that moved itself out of the group. The marker is also why two concurrent
//! execs in one Environment cannot cancel each other.
//!
//! There is no PTY in v0 (`pty` stays false): a command gets pipes, and stdin is
//! an optional bounded body.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::contract::{ExecChunk, ExecSession, ExecSpec};
use crate::error::{BackendError, Result};

use super::cli::Wsl;
use super::log::LogSink;
use super::paths;

/// The in-distro wrapper, written into every Project distro on `start`.
pub const EXEC_PATH: &str = "/usr/local/bin/agentify-exec";
/// The wrapper, compiled into the daemon: no shared mount, no host path.
pub const EXEC_SCRIPT: &str = include_str!("scripts/exec.sh");
/// The canceller, compiled in for the same reason.
pub const CANCEL_SCRIPT: &str = include_str!("scripts/cancel.sh");
/// Where the wrapper records process groups. A tmpfs path, so a distro restart
/// clears it and no state leaks between runs.
pub const EXEC_DIR: &str = "/run/agentify/exec";

/// `AGENTIFY_EXEC_MARKER`, the environment variable that makes an exec findable.
pub const MARKER_ENV: &str = "AGENTIFY_EXEC_MARKER";

/// A unique, shell-safe marker for one exec.
pub fn marker(environment: &str) -> String {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let prefix: String = environment
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{}-{}", prefix.trim_matches('-'), &unique[..12])
}

/// The argument vector the wrapper is started with.
///
/// Kept here, not inline, because it is the contract between two halves written
/// in two languages; [`tests::the_wrapper_is_invoked_with_a_marker_and_a_body`]
/// pins its shape.
pub fn argv(spec: &ExecSpec, marker: &str) -> Result<Vec<String>> {
    if spec.command.is_empty() {
        return Err(BackendError::ExecutionFailed {
            code: 2,
            detail: "a command needs at least one argument".to_string(),
        });
    }
    let mut argv = vec![
        EXEC_PATH.to_string(),
        "--marker".to_string(),
        marker.to_string(),
        "--workdir".to_string(),
        EXEC_DIR.to_string(),
    ];
    if let Some(cwd) = spec.cwd.as_deref().map(str::trim).filter(|raw| !raw.is_empty()) {
        argv.push("--cwd".to_string());
        argv.push(guest_cwd(cwd)?);
    }
    for (key, value) in &spec.env {
        if key.trim().is_empty() || key.contains('=') {
            return Err(BackendError::ExecutionFailed {
                code: 2,
                detail: format!("{key:?} is not a usable environment variable name"),
            });
        }
        argv.push("--env".to_string());
        argv.push(format!("{key}={value}"));
    }
    argv.push("--".to_string());
    argv.extend(spec.command.iter().cloned());
    Ok(argv)
}

/// A working directory inside the distro: absolute, and never one that climbs.
fn guest_cwd(raw: &str) -> Result<String> {
    let absolute = if raw.starts_with('/') {
        raw.to_string()
    } else {
        format!("/{raw}")
    };
    if absolute == "/" {
        return Ok(absolute);
    }
    paths::guest_path(&absolute)
}

/// The cancel script for one marker. The marker is `[a-z0-9-]`, so substituting
/// it into a single-quoted shell word cannot change the script.
pub fn cancel_script(marker: &str) -> String {
    debug_assert!(
        marker
            .chars()
            .all(|character| character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'),
        "a marker must be shell-safe: {marker}"
    );
    super::unix_script(CANCEL_SCRIPT).replace("__MARKER__", marker)
}

/// Terminating one exec, from wherever the caller happens to be.
///
/// Cloned into the session's `cancel` closure and (when the request carries a
/// timeout) into the watchdog thread, so either route stops the same command.
#[derive(Clone)]
pub struct Canceller {
    fired: Arc<AtomicBool>,
    wsl: Wsl,
    distro: String,
    marker: String,
    /// The Environment's log, so a cancellation that did not take is visible
    /// instead of silent. `None` when the caller has no log to hand.
    log: Option<Arc<Mutex<LogSink>>>,
}

impl Canceller {
    pub fn new(
        wsl: Wsl,
        distro: impl Into<String>,
        marker: impl Into<String>,
        log: Option<Arc<Mutex<LogSink>>>,
    ) -> Self {
        Self {
            fired: Arc::new(AtomicBool::new(false)),
            wsl,
            distro: distro.into(),
            marker: marker.into(),
            log,
        }
    }

    /// Tell the runtime to stop the command. Idempotent: the second caller gets
    /// `true` without a second kill.
    pub fn cancel(&self) -> bool {
        if self.fired.swap(true, Ordering::SeqCst) {
            return true;
        }
        let wsl = self.wsl.clone();
        let distro = self.distro.clone();
        let marker = self.marker.clone();
        let script = cancel_script(&marker);
        let log = self.log.clone();
        thread::spawn(move || {
            // On stdin, not as an argument: `wsl.exe` re-parses its command line,
            // and a multi-line script does not survive the trip. This is the same
            // channel the supervisor's own recipe uses.
            let result = wsl.run_with_stdin(&distro, &["sh", "-s"], script.as_bytes());
            let problem = match &result {
                Ok(output) if output.code != 0 => {
                    Some(format!("exit {}: {}", output.code, output.diagnostic()))
                }
                Ok(output) if !output.stderr_text().trim().is_empty() => {
                    Some(output.diagnostic())
                }
                Ok(_) => None,
                Err(err) => Some(err.to_string()),
            };
            if let Some(problem) = problem {
                if let Some(log) = log.as_ref() {
                    if let Ok(mut sink) = log.lock() {
                        sink.append(
                            super::SUPERVISOR_LOG_RESOURCE,
                            "stderr",
                            &format!("agentify-cancel {marker}: {problem}"),
                        );
                    }
                }
            }
        });
        true
    }
}

/// Start one command and hand back its stream.
///
/// Returns once the launcher exists: the chunks arrive as the command produces
/// them, and the stream always ends after an [`ExecChunk::Exit`] (also after a
/// cancellation, where the exit code is the launcher's own report of the
/// signalled command).
pub fn start(
    wsl: &Wsl,
    distro: &str,
    spec: &ExecSpec,
    marker: &str,
    log: Option<Arc<Mutex<LogSink>>>,
) -> Result<ExecSession> {
    let argv = argv(spec, marker)?;
    let mut child = wsl.spawn_streaming(distro, &argv)?;

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_reader = stdout.map(|pipe| {
        let sender = sender.clone();
        thread::spawn(move || pump(pipe, sender, ExecChunk::Stdout))
    });
    let stderr_reader = stderr.map(|pipe| {
        let sender = sender.clone();
        thread::spawn(move || pump(pipe, sender, ExecChunk::Stderr))
    });

    if let Some(body) = spec.stdin.clone() {
        if let Some(mut pipe) = child.stdin.take() {
            // A command that exits before reading its whole body closes the
            // pipe; that is its business, not an error here.
            thread::spawn(move || {
                let _ = pipe.write_all(&body);
            });
        }
    }

    // The exit chunk is published only after both pipes have drained, so a
    // caller that stops at `Exit` cannot lose the last line of output.
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let waiter = sender.clone();
    thread::spawn(move || {
        let mut child = child;
        let status = child.wait();
        if let Some(reader) = stdout_reader {
            let _ = reader.join();
        }
        if let Some(reader) = stderr_reader {
            let _ = reader.join();
        }
        finished.store(true, Ordering::SeqCst);
        let code = status.map(|status| status.code().unwrap_or(-1)).unwrap_or(-1);
        let _ = waiter.send(Ok(ExecChunk::Exit { code }));
    });
    drop(sender);

    let canceller = Canceller::new(wsl.clone(), distro, marker.to_string(), log);
    if let Some(timeout) = spec.timeout {
        watchdog(canceller.clone(), timeout, done);
    }

    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let cancel = canceller.clone();
    Ok(ExecSession::new(stream, move || cancel.cancel()))
}

/// Copy one pipe into the stream until it closes.
fn pump<R: Read>(
    mut reader: R,
    sender: tokio::sync::mpsc::UnboundedSender<Result<ExecChunk>>,
    wrap: fn(Vec<u8>) -> ExecChunk,
) {
    let mut buffer = vec![0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                if sender.send(Ok(wrap(buffer[..read].to_vec()))).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Cancel the exec when its request carried a timeout, unless it finished first.
fn watchdog(canceller: Canceller, timeout: Duration, done: Arc<AtomicBool>) {
    let started = Instant::now();
    thread::spawn(move || {
        let deadline = started + timeout;
        while Instant::now() < deadline {
            if done.load(Ordering::SeqCst) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            thread::sleep(left.min(Duration::from_millis(20)));
        }
        if !done.load(Ordering::SeqCst) {
            canceller.cancel();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(command: &[&str]) -> ExecSpec {
        ExecSpec {
            command: command.iter().map(|part| part.to_string()).collect(),
            ..ExecSpec::default()
        }
    }

    #[test]
    fn a_marker_is_unique_and_shell_safe() {
        let first = marker("Environment-1");
        let second = marker("Environment-1");
        assert_ne!(first, second, "two execs never share a marker");
        assert!(first.starts_with("environment-1-"), "{first}");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{first}"
        );
    }

    #[test]
    fn the_wrapper_is_invoked_with_a_marker_and_a_body() {
        let request = ExecSpec {
            command: vec!["echo".into(), "hello world".into()],
            cwd: Some("work".into()),
            env: BTreeMap::from([("GREETING".to_string(), "hi".to_string())]),
            ..ExecSpec::default()
        };
        let argv = argv(&request, "m-1").expect("argv");
        assert_eq!(
            argv,
            vec![
                EXEC_PATH,
                "--marker",
                "m-1",
                "--workdir",
                EXEC_DIR,
                "--cwd",
                "/work",
                "--env",
                "GREETING=hi",
                "--",
                "echo",
                "hello world",
            ]
        );
    }

    #[test]
    fn an_empty_command_and_a_climbing_cwd_are_refused() {
        assert_eq!(argv(&spec(&[]), "m").expect_err("empty").code(), "ExecutionFailed");

        let mut climbing = spec(&["ls"]);
        climbing.cwd = Some("../../etc".to_string());
        let err = argv(&climbing, "m").expect_err("climbs");
        assert!(err.to_string().contains("climbs out"), "{err}");

        let mut bad_env = spec(&["ls"]);
        bad_env.env.insert("NO=EQUALS".to_string(), "x".to_string());
        assert!(argv(&bad_env, "m").is_err());
    }

    #[test]
    fn the_root_as_a_cwd_is_allowed() {
        let mut request = spec(&["pwd"]);
        request.cwd = Some("/".to_string());
        assert!(argv(&request, "m").is_ok());
    }

    #[test]
    fn a_cancel_script_names_one_marker_and_nothing_else() {
        let script = cancel_script("env-1-abc123");
        assert!(script.contains("marker='env-1-abc123'"));
        assert!(!script.contains("__MARKER__"), "the placeholder is filled in");
        assert!(script.contains("pkill"), "pkill is the fast path when present");
        assert!(
            script.contains(&format!("{MARKER_ENV}=$marker")),
            "the marker sweep is the backstop"
        );
        assert!(
            !script.contains('\r'),
            "a CRLF script does not run on Linux"
        );
    }
}
