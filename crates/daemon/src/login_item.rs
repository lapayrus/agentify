//! The daemon's login item: a per-user Scheduled Task that starts `agentifyd` at
//! logon and restarts it if it dies.
//!
//! Never a Windows service: a service runs as `SYSTEM`, which would put the
//! daemon outside the user's trust boundary, hand it the wrong profile, and give
//! it the wrong `%LOCALAPPDATA%`. This is a task registered for the current user,
//! running with the current user's token at the least privilege level, so it
//! needs no elevation and no UAC prompt.
//!
//! The task is defined by XML rather than by `schtasks` flags because only the
//! XML form can ask for restart-on-failure, and it is registered with a
//! name the caller chooses — which is what lets a test install, observe, and
//! remove one under a unique name without touching the machine's real login item.
//!
//! ```sh
//! agentify login-item install     # register it for the current user
//! agentify login-item status      # is it registered, and for what?
//! agentify login-item remove      # take it out again
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// How many times Task Scheduler retries a failed run, and how often.
pub const RESTART_INTERVAL: &str = "PT1M";
pub const RESTART_COUNT: u32 = 3;

/// The name the shipped installer registers. Tests use their own.
pub const DEFAULT_TASK_NAME: &str = "Agentify Daemon";

/// What to run at logon, and what to call the task that runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginItem {
    /// Task Scheduler task name, shown to the user in Task Scheduler.
    pub name: String,
    /// The program to start: `agentifyd.exe`, or a launcher that wraps it.
    pub program: PathBuf,
    /// Arguments passed to it.
    pub args: Vec<String>,
    /// Working directory. Empty means Task Scheduler's default.
    pub working_dir: Option<PathBuf>,
}

impl LoginItem {
    /// The login item for the daemon binary beside the running executable.
    pub fn for_daemon(name: impl Into<String>) -> Result<Self> {
        let program = daemon_binary()?;
        Ok(Self {
            name: name.into(),
            program,
            args: Vec::new(),
            working_dir: None,
        })
    }

    /// A login item naming an explicit program: what a test uses, so it never
    /// has to touch the real daemon.
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            working_dir: None,
        }
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The command line as Task Scheduler stores it.
    pub fn command_line(&self) -> String {
        let mut line = quote(&self.program.to_string_lossy());
        for arg in &self.args {
            line.push(' ');
            line.push_str(&quote(arg));
        }
        line
    }

    /// The task definition, as Schedule Schema 1.2 XML.
    ///
    /// `user` is `DOMAIN\name`; the logon trigger and the principal both name it,
    /// so the task can only ever start as that user.
    pub fn xml(&self, user: &str) -> String {
        let working_dir = match &self.working_dir {
            Some(dir) => format!("<WorkingDirectory>{}</WorkingDirectory>", xml_escape(&dir.to_string_lossy())),
            None => String::new(),
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the Agentify daemon at logon and restarts it if it stops.</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>{restart_interval}</Interval>
      <Count>{restart_count}</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      {working_dir}
    </Exec>
  </Actions>
</Task>
"#,
            name = xml_escape(&self.name),
            user = xml_escape(user),
            restart_interval = RESTART_INTERVAL,
            restart_count = RESTART_COUNT,
            command = xml_escape(&self.program.to_string_lossy()),
            arguments = xml_escape(&self.args.join(" ")),
        )
    }
}

/// How a login item is registered, and whether it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginItemStatus {
    pub name: String,
    /// True when a task with this name exists.
    pub registered: bool,
    /// `schtasks /Query` output, trimmed, when it exists.
    pub detail: Option<String>,
}

#[cfg(windows)]
mod platform {
    use super::*;

    /// Register (or replace) the task for the current user. No elevation.
    pub fn install(item: &LoginItem) -> Result<()> {
        let user = current_user()?;
        let xml = item.xml(&user);
        let path = temp_xml(&item.name);
        // Task Scheduler wants UTF-16 with a byte-order mark.
        write_utf16(&path, &xml)
            .with_context(|| format!("writing the task definition {}", path.display()))?;

        let output = Command::new("schtasks.exe")
            .args(["/Create", "/TN", &item.name, "/XML"])
            .arg(&path)
            .args(["/F"])
            .output()
            .context("running schtasks.exe /Create")?;
        let _ = std::fs::remove_file(&path);
        if !output.status.success() {
            anyhow::bail!(
                "schtasks /Create for {:?} failed: {}",
                item.name,
                combined(&output)
            );
        }
        Ok(())
    }

    /// Remove the task. Removing a task that is not there is not an error: the
    /// caller asked for "not installed", and that is what it gets.
    pub fn remove(name: &str) -> Result<()> {
        let output = Command::new("schtasks.exe")
            .args(["/Delete", "/TN", name, "/F"])
            .output()
            .context("running schtasks.exe /Delete")?;
        if output.status.success() {
            return Ok(());
        }
        let text = combined(&output);
        // English and localized builds word this differently; both mention the
        // task being absent. Anything else is a real failure.
        if text.contains("cannot find")
            || text.contains("not find the task")
            || text.contains("does not exist")
            || text.contains("ERROR: The system cannot find")
        {
            return Ok(());
        }
        anyhow::bail!("schtasks /Delete for {name:?} failed: {text}");
    }

    /// Is the task registered, and what does it run?
    pub fn status(name: &str) -> Result<LoginItemStatus> {
        let Some(xml) = definition(name)? else {
            return Ok(LoginItemStatus {
                name: name.to_string(),
                registered: false,
                detail: None,
            });
        };
        let command = element(&xml, "Command").unwrap_or_else(|| "?".to_string());
        let arguments = element(&xml, "Arguments").unwrap_or_default();
        Ok(LoginItemStatus {
            name: name.to_string(),
            registered: true,
            detail: Some(format!("{command} {arguments}").trim().to_string()),
        })
    }

    /// The task definition Task Scheduler holds, or `None` when there is none.
    ///
    /// This is the OS-side truth: what a test asserts on, and what a person
    /// reads when the login item misbehaves.
    pub fn definition(name: &str) -> Result<Option<String>> {
        let output = Command::new("schtasks.exe")
            .args(["/Query", "/TN", name, "/XML"])
            .output()
            .context("running schtasks.exe /Query")?;
        if !output.status.success() {
            return Ok(None);
        }
        // The XML comes back as UTF-16 or as the console's code page depending on
        // the build.
        Ok(Some(decode(&output.stdout)))
    }

    /// `DOMAIN\name` for the current process user.
    pub fn current_user() -> Result<String> {
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let user = std::env::var("USERNAME")
            .context("USERNAME is not set, so the logon task has no principal")?;
        Ok(if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        })
    }

    fn temp_xml(name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        std::env::temp_dir().join(format!("agentify-login-item-{safe}-{}.xml", std::process::id()))
    }

    fn write_utf16(path: &Path, xml: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut bytes = Vec::with_capacity(xml.len() * 2 + 2);
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        for unit in xml.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let mut file = std::fs::File::create(path)?;
        file.write_all(&bytes)?;
        file.sync_all()
    }

    fn decode(bytes: &[u8]) -> String {
        if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
            let units: Vec<u16> = bytes[2..]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(bytes).to_string()
        }
    }

    fn element(xml: &str, tag: &str) -> Option<String> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = xml.find(&open)? + open.len();
        let end = xml[start..].find(&close)? + start;
        Some(unescape(xml[start..end].trim()))
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
}

#[cfg(unix)]
mod platform {
    use super::*;

    /// Unix is not a shipped platform in v0; these exist so the crate builds and
    /// so a test can assert the refusal rather than silently doing nothing.
    pub fn install(_item: &LoginItem) -> Result<()> {
        anyhow::bail!("the Agentify daemon's login item is a Windows Scheduled Task; Unix uses SMAppService on macOS")
    }

    pub fn remove(_name: &str) -> Result<()> {
        anyhow::bail!("the Agentify daemon's login item is a Windows Scheduled Task; Unix uses SMAppService on macOS")
    }

    pub fn status(name: &str) -> Result<LoginItemStatus> {
        Ok(LoginItemStatus {
            name: name.to_string(),
            registered: false,
            detail: None,
        })
    }

    pub fn definition(_name: &str) -> Result<Option<String>> {
        Ok(None)
    }

    pub fn current_user() -> Result<String> {
        Ok(std::env::var("USER").unwrap_or_else(|_| "user".to_string()))
    }
}

pub use platform::{current_user, definition, install, remove, status};

impl LoginItem {
    /// Register this login item for the current user. No elevation.
    pub fn install(&self) -> Result<()> {
        install(self)
    }

    /// Remove this login item.
    pub fn remove(&self) -> Result<()> {
        remove(&self.name)
    }

    /// Is this login item registered?
    pub fn status(&self) -> Result<LoginItemStatus> {
        status(&self.name)
    }
}

/// The daemon executable: beside the running binary, so an installed copy finds
/// its own sidecar and a development build finds `target/debug/agentifyd.exe`.
fn daemon_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    let dir = exe
        .parent()
        .context("the running executable has no parent directory")?;
    let name = if cfg!(windows) {
        "agentifyd.exe"
    } else {
        "agentifyd"
    };
    let beside = dir.join(name);
    if beside.is_file() {
        return Ok(beside);
    }
    // A CLI or shell binary lives in the same directory as `agentifyd` in a
    // development build; if it is not there, say where we looked.
    anyhow::bail!(
        "no {} beside {}; install the app or build the daemon first",
        name,
        exe.display()
    )
}

fn quote(value: &str) -> String {
    if value.contains(' ') || value.is_empty() {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LoginItem {
        LoginItem::new(
            "Agentify Daemon",
            PathBuf::from("C:\\Users\\someone\\AppData\\Local\\Programs\\Agentify\\agentifyd.exe"),
        )
        .with_args(["--data-root", "C:\\Users\\someone\\AppData\\Local\\Agentify"])
    }

    #[test]
    fn the_task_definition_runs_at_logon_as_the_current_user() {
        let xml = sample().xml("LAPTOP\\someone");
        assert!(xml.contains("<LogonTrigger>"), "the task starts at logon: {xml}");
        assert!(xml.contains("<UserId>LAPTOP\\someone</UserId>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(
            xml.contains("<RunLevel>LeastPrivilege</RunLevel>"),
            "a per-user task must not ask for elevation: {xml}"
        );
        assert!(
            !xml.contains("HighestAvailable"),
            "no elevation in the login item: {xml}"
        );
    }

    #[test]
    fn the_task_definition_restarts_a_crashed_daemon() {
        let xml = sample().xml("someone");
        assert!(xml.contains(&format!("<Interval>{RESTART_INTERVAL}</Interval>")));
        assert!(xml.contains(&format!("<Count>{RESTART_COUNT}</Count>")));
        assert!(
            xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            "the daemon is long-lived, so the task has no time limit: {xml}"
        );
        assert!(
            xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"),
            "a second logon must not start a second daemon: {xml}"
        );
    }

    #[test]
    fn the_command_line_quotes_paths_with_spaces() {
        let item = LoginItem::new("Agentify Daemon", PathBuf::from("C:\\Program Files\\Agentify\\agentifyd.exe"))
            .with_args(["start"]);
        assert_eq!(
            item.command_line(),
            "\"C:\\Program Files\\Agentify\\agentifyd.exe\" start"
        );
        assert_eq!(
            LoginItem::new("x", PathBuf::from("C:\\agentifyd.exe")).command_line(),
            "C:\\agentifyd.exe"
        );
    }

    #[test]
    fn the_command_and_arguments_are_xml_escaped() {
        let item = LoginItem::new("A & B", PathBuf::from("C:\\a&b\\agentifyd.exe"))
            .with_args(["--flag=<1>"]);
        let xml = item.xml("user");
        assert!(xml.contains("<Command>C:\\a&amp;b\\agentifyd.exe</Command>"), "{xml}");
        assert!(xml.contains("<Arguments>--flag=&lt;1&gt;</Arguments>"), "{xml}");
        assert!(xml.contains("<URI>\\A &amp; B</URI>"), "{xml}");
    }

    #[test]
    fn a_task_name_becomes_a_safe_temp_file_name() {
        let name = "Agentify Daemon test/1";
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        assert!(!safe.contains('/'));
        assert!(!safe.contains(' '));
    }

    #[test]
    fn the_login_item_is_never_a_service() {
        // The daemon's startup is a per-user task; the word "service" must not
        // appear anywhere in the definition, because a service would run as
        // SYSTEM and step outside the user's trust boundary.
        let xml = sample().xml("someone");
        assert!(!xml.to_ascii_lowercase().contains("service"));
    }
}
