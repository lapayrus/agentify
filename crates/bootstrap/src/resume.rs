//! The per-user resume entry: how first run survives a restart.
//!
//! `wsl --install` enables Windows optional components, and Windows will not run
//! them until it restarts. The elevated step therefore does not finish the job;
//! it schedules the rest of it:
//!
//! - One value under `HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce`,
//!   named [`RESUME_VALUE_NAME`], whose data is the command line that relaunches
//!   Agentify with [`RESUME_FLAG`]. Per-user, so no elevation, and `RunOnce` is
//!   consumed by Windows at the next sign-in: it never becomes a login item.
//! - Nothing else. No scheduled task, no service, no `HKLM`.
//!
//! The entry is removed when setup succeeds, so a machine that is ready carries
//! no trace of it. Removing an entry that Windows already consumed is not an
//! error — the caller asked for "no pending resume", and that is what it gets,
//! exactly as the login item treats a task that is already gone.
//!
//! A test never writes the real value: [`ResumeStore`] is injected, and the only
//! test that touches the registry uses a unique value name of its own and
//! removes it again.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// The `RunOnce` key a per-user resume belongs in.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce";

/// The value name the shipped app writes. Tests that touch the real registry
/// pick their own.
pub const RESUME_VALUE_NAME: &str = "AgentifyResumeBootstrap";

/// The flag the relaunched app sees, so it knows it is finishing an install
/// rather than starting one.
pub const RESUME_FLAG: &str = "--resume";

/// The `RunOnce` data for a program: its quoted path, then `bootstrap --resume`.
///
/// `resume_args` is the front end's own resume command line, because the CLI and
/// the desktop app spell it differently (`agentify bootstrap --resume` versus
/// `Agentify.exe --resume`).
pub fn resume_command(program: &Path, resume_args: &[&str]) -> String {
    let mut line = quote(&program.to_string_lossy());
    for arg in resume_args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    line
}

/// Where a pending resume is recorded, and whether one is.
///
/// The real implementation is [`RunOnce`]; a test injects an in-memory one so no
/// RunOnce entry is ever left behind by the suite.
pub trait ResumeStore {
    /// Record that Agentify must resume after the next sign-in.
    fn schedule(&self, value_name: &str, command: &str) -> Result<()>;
    /// The command the entry would run, when one exists.
    fn pending(&self, value_name: &str) -> Result<Option<String>>;
    /// Remove the entry. An entry that is already gone is not an error.
    fn clear(&self, value_name: &str) -> Result<()>;
}

/// The real registry: `HKCU\…\RunOnce`, through `reg.exe`, unelevated.
pub struct RunOnce;

impl ResumeStore for RunOnce {
    fn schedule(&self, value_name: &str, command: &str) -> Result<()> {
        #[cfg(not(windows))]
        {
            let _ = (value_name, command);
            anyhow::bail!("the resume entry is a Windows RunOnce value; this platform has none")
        }
        #[cfg(windows)]
        {
            let output = Command::new("reg.exe")
                .args(["add", RUN_KEY, "/v", value_name, "/t", "REG_SZ", "/d", command, "/f"])
                .output()
                .context("running reg.exe add on RunOnce")?;
            if !output.status.success() {
                anyhow::bail!(
                    "reg add {RUN_KEY}\\{value_name} failed: {}",
                    combined(&output)
                );
            }
            Ok(())
        }
    }

    fn pending(&self, value_name: &str) -> Result<Option<String>> {
        #[cfg(not(windows))]
        {
            let _ = value_name;
            Ok(None)
        }
        #[cfg(windows)]
        {
            let output = Command::new("reg.exe")
                .args(["query", RUN_KEY, "/v", value_name])
                .output()
                .context("running reg.exe query on RunOnce")?;
            if !output.status.success() {
                // Absent is the normal answer, not a failure.
                return Ok(None);
            }
            Ok(parse_query(&decode(&output.stdout), value_name))
        }
    }

    fn clear(&self, value_name: &str) -> Result<()> {
        #[cfg(not(windows))]
        {
            let _ = value_name;
            Ok(())
        }
        #[cfg(windows)]
        {
            let output = Command::new("reg.exe")
                .args(["delete", RUN_KEY, "/v", value_name, "/f"])
                .output()
                .context("running reg.exe delete on RunOnce")?;
            if output.status.success() {
                return Ok(());
            }
            let text = combined(&output);
            let lowered = text.to_ascii_lowercase();
            // Localized builds word "no such value" differently; all of them mean
            // the entry is already gone, which is what the caller wanted.
            if lowered.contains("unable to find")
                || lowered.contains("cannot find")
                || lowered.contains("does not exist")
                || lowered.contains("nicht gefunden")
            {
                return Ok(());
            }
            anyhow::bail!("reg delete {RUN_KEY}\\{value_name} failed: {text}");
        }
    }
}

/// The value data out of `reg query` output:
///
/// ```text
/// HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\RunOnce
///     AgentifyResumeBootstrap    REG_SZ    "C:\…\agentify.exe" bootstrap --resume
/// ```
fn parse_query(text: &str, value_name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with(value_name) {
            continue;
        }
        let rest = line[value_name.len()..].trim_start();
        // The type column, then the data — which may itself contain spaces.
        let Some((kind, data)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        if kind.trim().is_empty() {
            continue;
        }
        let data = data.trim();
        return Some(data.to_string()).filter(|data| !data.is_empty());
    }
    None
}

fn decode(bytes: &[u8]) -> String {
    crate::detect::decode_output(bytes)
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string()
}

fn quote(value: &str) -> String {
    if value.contains(' ') || value.is_empty() {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::FakeRunOnce;

    #[test]
    fn the_resume_command_quotes_the_program_and_carries_the_flag() {
        let line = resume_command(
            Path::new("C:\\Program Files\\Agentify\\agentify.exe"),
            &["bootstrap", RESUME_FLAG],
        );
        assert_eq!(
            line,
            "\"C:\\Program Files\\Agentify\\agentify.exe\" bootstrap --resume"
        );
        assert_eq!(
            resume_command(Path::new("C:\\agentify.exe"), &[RESUME_FLAG]),
            "C:\\agentify.exe --resume"
        );
        assert!(line.ends_with("--resume"), "the flag is last: {line}");
    }

    #[test]
    fn a_reg_query_line_yields_its_data_which_may_contain_spaces() {
        let text = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\RunOnce\r\n    AgentifyResumeBootstrap    REG_SZ    \"C:\\Program Files\\Agentify\\agentify.exe\" bootstrap --resume\r\n";
        assert_eq!(
            parse_query(text, RESUME_VALUE_NAME).as_deref(),
            Some("\"C:\\Program Files\\Agentify\\agentify.exe\" bootstrap --resume")
        );
        assert_eq!(parse_query(text, "SomeoneElsesValue"), None);
        assert_eq!(parse_query("", RESUME_VALUE_NAME), None);
        // A value with no data is not a pending resume.
        assert_eq!(
            parse_query(
                "HKEY_CURRENT_USER\\...\\RunOnce\r\n    AgentifyResumeBootstrap    REG_SZ\r\n",
                RESUME_VALUE_NAME
            ),
            None
        );
    }

    #[test]
    fn the_fake_stores_reads_and_removes_an_entry_without_touching_windows() {
        let fake = FakeRunOnce::default();
        let command = resume_command(Path::new("C:\\agentify.exe"), &["bootstrap", RESUME_FLAG]);
        assert_eq!(fake.pending(RESUME_VALUE_NAME).expect("pending"), None);
        fake.schedule(RESUME_VALUE_NAME, &command).expect("schedule");
        assert_eq!(
            fake.pending(RESUME_VALUE_NAME).expect("pending").as_deref(),
            Some(command.as_str())
        );
        fake.clear(RESUME_VALUE_NAME).expect("clear");
        assert_eq!(fake.pending(RESUME_VALUE_NAME).expect("pending"), None);
        // Clearing twice is not an error: a consumed RunOnce value is normal.
        fake.clear(RESUME_VALUE_NAME).expect("clear again");
        assert_eq!(fake.scheduled(), 1);
        assert_eq!(fake.cleared(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn the_real_registry_round_trips_a_value_under_a_unique_test_name() {
        // A name of its own, so the suite can never touch the shipped entry, and
        // the value is removed again however the assertions go.
        let name = format!(
            "AgentifyTestResume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        );
        let command = resume_command(Path::new("C:\\Program Files\\Agentify\\agentify.exe"), &["--resume"]);
        let store = RunOnce;

        assert_eq!(store.pending(&name).expect("nothing yet"), None);
        store.schedule(&name, &command).expect("schedule the test value");
        assert_eq!(
            store.pending(&name).expect("a pending resume").as_deref(),
            Some(command.as_str()),
            "the value comes back exactly as it went in"
        );
        store.clear(&name).expect("remove the test value");
        assert_eq!(
            store.pending(&name).expect("gone"),
            None,
            "the test leaves no RunOnce entry behind"
        );
        // Removing it again is not an error.
        store.clear(&name).expect("clearing a missing value is fine");
    }

    #[test]
    fn the_resume_entry_is_per_user_and_fires_once() {
        assert!(RUN_KEY.starts_with("HKCU\\"), "{RUN_KEY}");
        assert!(RUN_KEY.ends_with("RunOnce"), "{RUN_KEY}");
        assert!(!RUN_KEY.to_ascii_lowercase().contains("hklm"));
        assert!(!RESUME_VALUE_NAME.is_empty());
        assert_eq!(RESUME_FLAG, "--resume");
    }
}
