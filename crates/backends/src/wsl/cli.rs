//! The `wsl.exe` command surface, wrapped.
//!
//! Everything the adapter does to the machine goes through here: it lists
//! distros, imports and unregisters them, runs a command in one, and holds a
//! foreground child open. Raw stderr never decides control flow — a non-zero
//! exit is a typed error, and the text is carried only for a human to read.
//!
//! Two rules are enforced by construction, not by comment:
//!
//! * only distros whose name the adapter minted are ever terminated or
//!   unregistered ([`Wsl::owns`] checks the prefix), so `Ubuntu` and
//!   `docker-desktop` cannot be touched;
//! * `wsl --shutdown` is never run. It kills every distro on the machine,
//!   including the user's own; this module has no function for it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::error::{BackendError, Result};

/// Executable the adapter drives. Named without a path so it resolves through
/// `PATH`; WSL ships it in `System32`.
pub const WSL_EXE: &str = "wsl.exe";

/// One registered distro, as `wsl -l -v` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    /// `NAME` as registered, lower-cased by WSL.
    pub name: String,
    /// `Running`, `Stopped`, `Installing`, ... exactly as WSL prints it.
    ///
    /// WSL translates this column, so it is for humans. Liveness is decided by
    /// [`Wsl::is_running`], which asks WSL for the *names* of running distros
    /// rather than reading a word that may be in another language.
    pub state: String,
    /// WSL version: `1` or `2`.
    pub version: String,
}

impl Distro {
    pub fn is_running(&self) -> bool {
        self.state.eq_ignore_ascii_case("running")
    }
}

/// Is this process able to drive WSL at all?
pub fn is_supported() -> bool {
    cfg!(windows)
}

fn unsupported() -> BackendError {
    BackendError::BackendUnavailable {
        detail: "the WSL Backend only runs on Windows".to_string(),
    }
}

/// A command result, with the two streams kept apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Raw stdout bytes. A text command is usually UTF-8; WSL may prepend a
    /// UTF-16 BOM, which [`Output::stdout_text`] strips.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

impl Output {
    pub fn stdout_text(&self) -> String {
        decode(&self.stdout)
    }

    pub fn stderr_text(&self) -> String {
        decode(&self.stderr)
    }

    /// One bounded line of diagnostic text, for a typed error's `detail`.
    /// Never parsed: a failure is classified by exit status and by the shape of
    /// what was asked, not by matching prose.
    pub fn diagnostic(&self) -> String {
        let raw = if self.stderr_text().trim().is_empty() {
            self.stdout_text()
        } else {
            self.stderr_text()
        };
        let first = raw
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        let mut text: String = first.chars().take(200).collect();
        if text.len() < first.len() {
            text.push('…');
        }
        text
    }
}

/// WSL speaks UTF-16LE for its own messages and UTF-8 for a command's output.
/// A BOM is the tell; without one the bytes are taken as UTF-8.
fn decode(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) || looks_utf16(bytes) {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let text = String::from_utf16_lossy(&units);
        text.trim_start_matches('\u{feff}').to_string()
    } else {
        String::from_utf8_lossy(bytes)
            .trim_start_matches('\u{feff}')
            .to_string()
    }
}

/// WSL writes UTF-16LE without a BOM for `-l -v`. A NUL in the first few bytes
/// is the reliable signal, and no command output this adapter reads is UTF-16
/// for any other reason.
fn looks_utf16(bytes: &[u8]) -> bool {
    bytes.iter().take(8).any(|byte| *byte == 0)
}

/// The adapter's handle on the `wsl.exe` CLI, scoped to one distro-name prefix.
#[derive(Debug, Clone)]
pub struct Wsl {
    /// Distro names the adapter is allowed to terminate and unregister, as a
    /// prefix (with its trailing `-`).
    pub prefix: String,
}

impl Wsl {
    pub fn new(prefix: impl Into<String>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.ends_with('-') {
            prefix.push('-');
        }
        Self { prefix }
    }

    /// Is this a distro the adapter minted? Everything else — `Ubuntu`,
    /// `docker-desktop`, a distro the user created — is off limits.
    pub fn owns(&self, distro: &str) -> bool {
        distro.starts_with(&self.prefix)
    }

    /// Every registered distro, in `wsl -l -v` order.
    pub fn list(&self) -> Result<Vec<Distro>> {
        if !is_supported() {
            return Err(unsupported());
        }
        let output = self.raw(&["--list".to_string(), "--verbose".to_string()])?;
        // `wsl -l -v` exits 0 with a message on stderr when no distro is
        // registered; treat that as an empty list, not a failure.
        if output.code != 0 {
            if output.stdout_text().trim().is_empty() {
                return Ok(Vec::new());
            }
            return Err(BackendError::BackendUnavailable {
                detail: format!("`wsl -l -v` failed: {}", output.diagnostic()),
            });
        }
        Ok(parse_list(&output.stdout_text()))
    }

    /// One distro's row, if it is registered.
    pub fn find(&self, name: &str) -> Result<Option<Distro>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|distro| distro.name.eq_ignore_ascii_case(name)))
    }

    /// Names of the distros that are running right now.
    ///
    /// `wsl --list --running --quiet` prints one name per line and nothing
    /// else, which is the only part of WSL's table that is not translated.
    pub fn running(&self) -> Result<Vec<String>> {
        if !is_supported() {
            return Err(unsupported());
        }
        let output = self.raw(&[
            "--list".to_string(),
            "--running".to_string(),
            "--quiet".to_string(),
        ])?;
        if output.code != 0 {
            if output.stdout_text().trim().is_empty() {
                return Ok(Vec::new());
            }
            return Err(BackendError::BackendUnavailable {
                detail: format!("`wsl -l --running -q` failed: {}", output.diagnostic()),
            });
        }
        Ok(parse_names(&output.stdout_text()))
    }

    /// Is this distro running? Asked of WSL, not inferred from a table.
    pub fn is_running(&self, name: &str) -> Result<bool> {
        Ok(self
            .running()?
            .iter()
            .any(|running| running.eq_ignore_ascii_case(name)))
    }

    /// `wsl --import <name> <dir> <tar> --version 2`.
    pub fn import(&self, name: &str, dir: &Path, rootfs: &Path) -> Result<()> {
        if !is_supported() {
            return Err(unsupported());
        }
        let output = self.raw(&[
            "--import".into(),
            name.to_string(),
            dir.display().to_string(),
            rootfs.display().to_string(),
            "--version".into(),
            "2".into(),
        ])?;
        if output.code != 0 {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "`wsl --import {name}` failed (exit {}): {}",
                    output.code,
                    output.diagnostic()
                ),
            });
        }
        Ok(())
    }

    /// `wsl --export <name> <path>` — the golden template is one of these.
    pub fn export(&self, name: &str, path: &Path) -> Result<()> {
        if !is_supported() {
            return Err(unsupported());
        }
        let output = self.raw(&[
            "--export".into(),
            name.to_string(),
            path.display().to_string(),
        ])?;
        if output.code != 0 {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "`wsl --export {name}` failed (exit {}): {}",
                    output.code,
                    output.diagnostic()
                ),
            });
        }
        Ok(())
    }

    /// `wsl --terminate <name>`: kills this distro's processes, keeps its disk.
    /// Refuses a distro it did not mint.
    pub fn terminate(&self, name: &str) -> Result<()> {
        if !self.owns(name) {
            return Err(BackendError::BackendUnavailable {
                detail: format!("refusing to terminate {name:?}: it is not one of this daemon's distros"),
            });
        }
        let output = self.raw(&["--terminate".into(), name.to_string()])?;
        if output.code != 0 {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "`wsl --terminate {name}` failed (exit {}): {}",
                    output.code,
                    output.diagnostic()
                ),
            });
        }
        Ok(())
    }

    /// `wsl --unregister <name>`: removes the registration *and* the disk.
    /// Refuses a distro it did not mint.
    pub fn unregister(&self, name: &str) -> Result<()> {
        if !self.owns(name) {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "refusing to unregister {name:?}: it is not one of this daemon's distros"
                ),
            });
        }
        let output = self.raw(&["--unregister".into(), name.to_string()])?;
        if output.code != 0 {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "`wsl --unregister {name}` failed (exit {}): {}",
                    output.code,
                    output.diagnostic()
                ),
            });
        }
        Ok(())
    }

    /// Run a command as root in one distro, capturing both streams.
    pub fn run(&self, distro: &str, argv: &[&str]) -> Result<Output> {
        if !is_supported() {
            return Err(unsupported());
        }
        let mut args = vec!["-d".to_string(), distro.to_string()];
        args.push("--".to_string());
        args.extend(argv.iter().map(|part| part.to_string()));
        self.raw(&args)
    }

    /// Run a command with a body on stdin. Used to write the in-distro helper
    /// without depending on a shared mount.
    pub fn run_with_stdin(&self, distro: &str, argv: &[&str], stdin: &[u8]) -> Result<Output> {
        if !is_supported() {
            return Err(unsupported());
        }
        let mut command = Command::new(WSL_EXE);
        command
            .arg("-d")
            .arg(distro)
            .arg("--")
            .args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut command);
        let mut child = command.spawn().map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot run `{WSL_EXE}`: {err}"),
        })?;
        if let Some(mut pipe) = child.stdin.take() {
            // A command that exits before reading its whole stdin closes the
            // pipe; that is its business, not an error here.
            let _ = pipe.write_all(stdin);
        }
        collect(child)
    }

    /// Write bytes to a path inside a distro. `sh` is the only dependency: the
    /// base rootfs always has it.
    pub fn write_file(&self, distro: &str, path: &str, contents: &[u8], mode: &str) -> Result<()> {
        let script = format!("cat > {path} && chmod {mode} {path}");
        let output = self.run_with_stdin(distro, &["sh", "-c", &script], contents)?;
        if output.code != 0 {
            return Err(BackendError::ExecutionFailed {
                code: output.code,
                detail: format!(
                    "cannot write {path} in {distro}: {}",
                    output.diagnostic()
                ),
            });
        }
        Ok(())
    }

    /// Read a path inside a distro. `Ok(None)` when it does not exist.
    pub fn read_file(&self, distro: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let output = self.run(distro, &["sh", "-c", &format!("test -f {path} && cat {path}")])?;
        if output.code != 0 {
            return Ok(None);
        }
        Ok(Some(output.stdout))
    }

    /// Hold a *foreground* `wsl.exe` child open whose payload is the supervisor.
    ///
    /// This is the shape the platform forces: a background process launched by
    /// a one-shot `wsl.exe` is reaped when that launcher exits, so the daemon
    /// keeps one launcher alive per running Environment for as long as the
    /// Environment runs.
    pub fn spawn_foreground(&self, distro: &str, argv: &[&str]) -> Result<Child> {
        if !is_supported() {
            return Err(unsupported());
        }
        let mut command = Command::new(WSL_EXE);
        command
            .arg("-d")
            .arg(distro)
            .arg("--")
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut command);
        command.spawn().map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot start the supervisor in {distro}: {err}"),
        })
    }

    fn raw(&self, args: &[String]) -> Result<Output> {
        self.raw_in(None, args)
    }

    fn raw_in(&self, stdin: Option<&[u8]>, args: &[String]) -> Result<Output> {
        let mut command = Command::new(WSL_EXE);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut command);
        let mut child = command.spawn().map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot run `{WSL_EXE}`: {err}"),
        })?;
        if let (Some(body), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(body);
        }
        collect(child)
    }
}

/// Read both pipes to the end and wait for the status.
fn collect(mut child: Child) -> Result<Output> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    let status = child.wait().map_err(|err| BackendError::BackendUnavailable {
        detail: format!("`{WSL_EXE}` did not report a status: {err}"),
    })?;
    Ok(Output {
        stdout,
        stderr,
        code: status.code().unwrap_or(-1),
    })
}

/// A daemon holds no console; a child that opens one flashes a window and can
/// steal focus. `CREATE_NO_WINDOW` keeps it invisible.
fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Parse the table `wsl -l -v` prints.
///
/// The header and the state column are translated, so neither is trusted: a row
/// is accepted only when its *last* column is a WSL version (`1` or `2`), which
/// is a number in every locale, and the name is the first column — WSL distro
/// names never contain a space. Anything else (a header, a prose message) has
/// no such column and is skipped.
pub fn parse_list(text: &str) -> Vec<Distro> {
    let mut distros = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches('*').trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let version = parts[parts.len() - 1];
        if version != "1" && version != "2" {
            continue;
        }
        distros.push(Distro {
            name: parts[0].to_string(),
            state: parts[1..parts.len() - 1].join(" "),
            version: version.to_string(),
        });
    }
    distros
}

/// `wsl -l --running -q` prints bare names, one per line.
pub fn parse_names(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}


/// Lines a supervisor writes on stdout, and what the daemon does with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorLine {
    /// `agentify-supervise: ready <port>`
    Bound(u16),
    /// `agentify-supervise: fail <port> <reason>`
    Failed(u16, String),
    /// The supervisor bound everything it was asked to.
    Ready,
    /// Anything else: kept for a log, never parsed for control flow.
    Other(String),
}

/// The supervisor's stdout protocol. It is *our* protocol, so parsing it is not
/// the same as parsing `wsl.exe`'s stderr: this text is produced by code this
/// repository ships, and the words are a contract between the two halves.
pub fn parse_supervisor_line(line: &str) -> SupervisorLine {
    const MARKER: &str = "agentify-supervise:";
    let Some(rest) = line.trim().strip_prefix(MARKER) else {
        return SupervisorLine::Other(line.to_string());
    };
    let mut parts = rest.split_whitespace();
    match parts.next() {
        Some("ready") => match parts.next() {
            Some(port) => match port.parse::<u16>() {
                Ok(port) => SupervisorLine::Bound(port),
                Err(_) => SupervisorLine::Other(line.to_string()),
            },
            None => SupervisorLine::Ready,
        },
        Some("fail") => {
            let port = parts.next().and_then(|raw| raw.parse::<u16>().ok());
            let reason = parts.collect::<Vec<_>>().join(" ");
            match port {
                Some(port) => SupervisorLine::Failed(port, reason),
                None => SupervisorLine::Other(line.to_string()),
            }
        }
        _ => SupervisorLine::Other(line.to_string()),
    }
}

/// Drain a child's stdout on a background thread, so the pipe never fills and
/// the daemon can read the protocol as it arrives.
pub fn drain_lines<R: Read + Send + 'static>(
    reader: R,
    sink: std::sync::mpsc::Sender<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if sink.send(line).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_parsed_name_state_version() {
        // The bytes WSL actually prints for a two-distro machine, decoded.
        let text = "  NAME              STATE           VERSION\n* Ubuntu            Running         2\n  docker-desktop    Stopped         2\n";
        let distros = parse_list(text);
        assert_eq!(
            distros,
            vec![
                Distro {
                    name: "Ubuntu".into(),
                    state: "Running".into(),
                    version: "2".into()
                },
                Distro {
                    name: "docker-desktop".into(),
                    state: "Stopped".into(),
                    version: "2".into()
                },
            ]
        );
    }

    #[test]
    fn an_empty_listing_is_an_empty_list() {
        assert!(parse_list("").is_empty());
        assert!(parse_list("   \n").is_empty());
        assert!(parse_list("Windows Subsystem for Linux has no installed distributions.").is_empty());
    }

    #[test]
    fn utf16_listing_output_is_decoded() {
        let text = "  NAME      STATE     VERSION\r\n* Ubuntu    Running   2\r\n";
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode(&bytes);
        assert!(decoded.contains("Ubuntu"));
        assert_eq!(parse_list(&decoded)[0].name, "Ubuntu");
    }

    #[test]
    fn only_our_own_distros_are_owned() {
        let wsl = Wsl::new("agentify-t7");
        assert!(wsl.owns("agentify-t7-abc"));
        assert!(!wsl.owns("Ubuntu"));
        assert!(!wsl.owns("docker-desktop"));
        assert!(!wsl.owns("agentify"));
    }

    #[test]
    fn the_supervisor_protocol_is_parsed() {
        assert_eq!(
            parse_supervisor_line("agentify-supervise: ready 41000"),
            SupervisorLine::Bound(41000)
        );
        assert_eq!(
            parse_supervisor_line("agentify-supervise: ready"),
            SupervisorLine::Ready
        );
        assert_eq!(
            parse_supervisor_line("agentify-supervise: fail 41000 address in use"),
            SupervisorLine::Failed(41000, "address in use".to_string())
        );
        assert_eq!(
            parse_supervisor_line("whatever else is on stdout"),
            SupervisorLine::Other("whatever else is on stdout".to_string())
        );
    }
}
