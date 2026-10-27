//! The one elevated step, and the only place its command line is built.
//!
//! Enabling WSL is the single thing Agentify cannot do as the signed-in user:
//! `wsl --install --no-distribution` turns on the two Windows optional
//! components and needs an administrator token. Everything else — importing the
//! pinned rootfs, unregistering a distro, running commands — works unelevated.
//!
//! So there is exactly one command, [`ElevatedCommand`], built by exactly one
//! function, [`plan`], and run by exactly one implementation, [`UacElevator`]:
//!
//! ```text
//! powershell.exe -NoProfile -NonInteractive -Command
//!   $ErrorActionPreference='Stop';
//!   $p=Start-Process -FilePath 'wsl.exe'
//!                    -ArgumentList '--install --no-distribution'
//!                    -Verb RunAs -Wait -PassThru;
//!   exit $p.ExitCode
//! ```
//!
//! `-Verb RunAs` is what raises the UAC prompt: Windows shows it, the human
//! answers it, and `-Wait` makes the parent wait for the elevated process to
//! exit so the outcome is known rather than assumed. The child's exit code is
//! returned unchanged, so `3010` (`ERROR_SUCCESS_REBOOT_REQUIRED`) is
//! distinguishable from a failure and from a declined prompt.
//!
//! A test never runs this: [`Elevator`] is injected, and the only fake a test
//! uses records the command instead of launching it.

use std::process::Command;

use anyhow::Result;

use crate::detect::Detection;

/// A command to run, elevated, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElevatedCommand {
    /// The executable, e.g. `wsl.exe`.
    pub program: String,
    /// Its arguments, in order.
    pub args: Vec<String>,
}

impl ElevatedCommand {
    /// Enable the two WSL optional components, and install no distribution: the
    /// pinned Ubuntu rootfs is imported later, unelevated, from the local cache.
    pub fn install_wsl() -> Self {
        Self {
            program: "wsl.exe".to_string(),
            args: vec!["--install".to_string(), "--no-distribution".to_string()],
        }
    }

    /// Make WSL 2 what new distros are created on, for a machine where WSL is
    /// already enabled but the default version is 1.
    pub fn set_default_version_two() -> Self {
        Self {
            program: "wsl.exe".to_string(),
            args: vec!["--set-default-version".to_string(), "2".to_string()],
        }
    }

    /// The elevated command itself, as Windows will see it.
    pub fn command_line(&self) -> String {
        let mut line = quote(&self.program);
        for arg in &self.args {
            line.push(' ');
            line.push_str(&quote(arg));
        }
        line
    }

    /// The arguments for the *unelevated* `powershell.exe` that asks for UAC.
    ///
    /// One string, no double quotes: it survives being passed as a single
    /// argument through `Command`, and it is what the tests assert on.
    pub fn uac_arguments(&self) -> Vec<String> {
        let script = format!(
            "$ErrorActionPreference='Stop';$p=Start-Process -FilePath {program} -ArgumentList {arguments} \
             -Verb RunAs -Wait -PassThru;if($p){{exit $p.ExitCode}}else{{exit 1}}",
            program = single_quoted(&self.program),
            arguments = single_quoted(&arguments_joined(&self.args)),
        );
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ]
    }

    /// The whole thing a person could paste into a terminal to do it by hand.
    pub fn to_string_for_humans(&self) -> String {
        format!(
            "powershell.exe {}",
            self.uac_arguments().join(" ")
        )
    }
}

/// What an elevated run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElevationOutcome {
    /// The elevated process's exit code, unchanged.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ElevationOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }

    /// Windows' `ERROR_SUCCESS_REBOOT_REQUIRED`: the work is done and a restart
    /// has to finish it.
    pub fn reboot_required(&self) -> bool {
        self.exit_code == 3010
    }

    /// The human answered No at the UAC prompt. PowerShell reports this as its
    /// own failure, with the reason on stderr.
    pub fn declined(&self) -> bool {
        if self.exit_code == 0 {
            return false;
        }
        let stderr = self.stderr.to_ascii_lowercase();
        stderr.contains("canceled by the user")
            || stderr.contains("cancelled by the user")
            || stderr.contains("the operation was canceled")
            || stderr.contains("the operation was cancelled")
    }

    /// One sentence, naming the code and whatever the command said.
    pub fn failure(&self) -> String {
        let detail = if self.stderr.trim().is_empty() {
            self.stdout.trim().to_string()
        } else {
            self.stderr.trim().to_string()
        };
        let code = if self.reboot_required() {
            " (3010: a restart is required)".to_string()
        } else {
            format!(" (exit code {})", self.exit_code)
        };
        if detail.is_empty() {
            format!("the elevated step failed{code}")
        } else {
            format!("the elevated step failed{code}: {detail}")
        }
    }
}

/// Runs one elevated command and waits for it.
///
/// The only implementation in production is [`UacElevator`]; a test injects a
/// fake so nothing is ever installed, prompted, or elevated.
pub trait Elevator {
    fn run(&self, command: &ElevatedCommand) -> Result<ElevationOutcome>;
}

/// The real one: `powershell.exe` asking Windows for an administrator token.
pub struct UacElevator;

impl Elevator for UacElevator {
    fn run(&self, command: &ElevatedCommand) -> Result<ElevationOutcome> {
        #[cfg(not(windows))]
        {
            let _ = command;
            anyhow::bail!("Agentify elevates through UAC, which only exists on Windows")
        }
        #[cfg(windows)]
        {
            let output = Command::new("powershell.exe")
                .args(command.uac_arguments())
                .output()
                .map_err(|err| anyhow::anyhow!("launching the elevated step: {err}"))?;
            Ok(ElevationOutcome {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: crate::detect::decode_output(&output.stdout),
                stderr: crate::detect::decode_output(&output.stderr),
            })
        }
    }
}

/// What has to happen, in one elevated step, and whether it ends in a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub step: ElevatedCommand,
    /// True when Windows has to restart before WSL works.
    pub reboot_expected: bool,
    /// Why this step is the one that is needed.
    pub reason: &'static str,
}

/// The one elevated step this machine still needs, or `None` when it needs none.
///
/// Nothing is installed and nothing is enabled here: this only decides what a
/// human would be asked to approve.
pub fn plan(detection: &Detection) -> Option<Plan> {
    if detection.usable() {
        return None;
    }
    if !detection.available {
        return Some(Plan {
            step: ElevatedCommand::install_wsl(),
            reboot_expected: true,
            reason: "WSL is not installed; enabling it turns on the two Windows optional \
                     components and needs a restart",
        });
    }
    if detection.default_version != Some(2) {
        return Some(Plan {
            step: ElevatedCommand::set_default_version_two(),
            reboot_expected: false,
            reason: "WSL is installed but new distros default to version 1; WSL 2 is what \
                     Agentify requires",
        });
    }
    // WSL 2 is present and default, so `usable()` would have been true.
    None
}

fn arguments_joined(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A PowerShell single-quoted literal: the only escaping is `''`.
fn single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A Windows command-line argument: quoted when it contains a space.
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
    use crate::detect::{builder, Distro};

    fn ubuntu() -> Vec<Distro> {
        vec![Distro {
            name: "Ubuntu".to_string(),
            state: "Running".to_string(),
            version: 2,
            default: true,
        }]
    }

    #[test]
    fn a_machine_without_wsl_gets_one_install_step_and_no_distribution() {
        let plan = plan(&builder(false, None, Vec::new())).expect("a plan");
        assert_eq!(plan.step, ElevatedCommand::install_wsl());
        assert_eq!(plan.step.command_line(), "wsl.exe --install --no-distribution");
        assert!(plan.reboot_expected, "enabling the components needs a restart");
        assert!(plan.reason.contains("not installed"), "{}", plan.reason);
    }

    #[test]
    fn a_wsl1_default_gets_only_the_default_version_step() {
        let plan = plan(&builder(true, Some(1), ubuntu())).expect("a plan");
        assert_eq!(
            plan.step.command_line(),
            "wsl.exe --set-default-version 2"
        );
        assert!(!plan.reboot_expected);
        assert!(plan.reason.contains("version 1"), "{}", plan.reason);
    }

    #[test]
    fn a_ready_machine_is_asked_for_nothing() {
        assert_eq!(plan(&builder(true, Some(2), ubuntu())), None);
    }

    #[test]
    fn the_elevated_step_is_one_command_line_in_one_place() {
        let command = ElevatedCommand::install_wsl();
        assert_eq!(command.program, "wsl.exe");
        assert_eq!(command.args, ["--install", "--no-distribution"]);
        assert_eq!(command.command_line(), "wsl.exe --install --no-distribution");
        assert_eq!(
            ElevatedCommand::set_default_version_two().command_line(),
            "wsl.exe --set-default-version 2"
        );
    }

    #[test]
    fn the_uac_launcher_asks_windows_for_an_administrator_token_and_waits() {
        let arguments = ElevatedCommand::install_wsl().uac_arguments();
        assert_eq!(arguments[0], "-NoProfile");
        assert_eq!(arguments[1], "-NonInteractive");
        assert_eq!(arguments[2], "-Command");
        let script = &arguments[3];
        assert!(script.contains("-Verb RunAs"), "UAC is the point: {script}");
        assert!(script.contains("-Wait"), "the parent waits: {script}");
        assert!(script.contains("-PassThru"), "the exit code is kept: {script}");
        assert!(
            script.contains("-FilePath 'wsl.exe'"),
            "the elevated program is named: {script}"
        );
        assert!(
            script.contains("-ArgumentList '--install --no-distribution'"),
            "the elevated arguments are named exactly once: {script}"
        );
        assert!(
            script.contains("exit $p.ExitCode"),
            "the child's code is the parent's code: {script}"
        );
        // A UAC prompt cannot be answered by a script running non-interactively,
        // and a double quote would break the single-argument pass-through.
        assert!(!script.contains('"'), "{script}");
    }

    #[test]
    fn a_quoted_program_and_embedded_quotes_survive_the_command_line() {
        let command = ElevatedCommand {
            program: "C:\\Program Files\\WSL\\wsl.exe".to_string(),
            args: vec!["--install".to_string(), "--user-root".to_string(), "C:\\a b".to_string()],
        };
        assert_eq!(
            command.command_line(),
            "\"C:\\Program Files\\WSL\\wsl.exe\" --install --user-root \"C:\\a b\""
        );
        let script = &command.uac_arguments()[3];
        assert!(
            script.contains("-FilePath 'C:\\Program Files\\WSL\\wsl.exe'"),
            "{script}"
        );
        assert!(
            script.contains("-ArgumentList '--install --user-root \"C:\\a b\"'"),
            "{script}"
        );

        let apostrophe = ElevatedCommand {
            program: "it's.exe".to_string(),
            args: Vec::new(),
        };
        assert!(apostrophe.uac_arguments()[3].contains("'it''s.exe'"));
    }

    #[test]
    fn a_for_human_line_is_the_paste_it_back_form() {
        let line = ElevatedCommand::install_wsl().to_string_for_humans();
        assert!(line.starts_with("powershell.exe -NoProfile"), "{line}");
        assert!(line.contains("wsl.exe"), "{line}");
    }

    #[test]
    fn a_zero_exit_is_success_and_3010_means_a_restart_will_finish_it() {
        let ok = ElevationOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(ok.succeeded());
        assert!(!ok.reboot_required());
        assert!(!ok.declined());

        let reboot = ElevationOutcome {
            exit_code: 3010,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(!reboot.succeeded());
        assert!(reboot.reboot_required());
        assert!(reboot.failure().contains("3010"), "{}", reboot.failure());
    }

    #[test]
    fn a_declined_prompt_is_recognised_instead_of_reported_as_a_failure() {
        let declined = ElevationOutcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Start-Process : This command cannot be run due to the error: The operation was canceled by the user.".to_string(),
        };
        assert!(declined.declined());
        assert!(!declined.succeeded());

        let failed = ElevationOutcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "The Windows Subsystem for Linux has not been enabled.".to_string(),
        };
        assert!(!failed.declined(), "a real failure is not a declination");
        assert!(failed.failure().contains("exit code 1"), "{}", failed.failure());
    }
}
