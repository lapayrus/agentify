//! The per-user `PATH` entry that names the Agentify programs.
//!
//! The installer puts its program directory on `HKCU\Environment\Path`, so an
//! MCP client can be configured with `agentify-mcp` instead of an absolute path
//! through the user's profile. Removal takes out exactly the entry the installer
//! added: entries the user changed are left alone.
//!
//! The value is edited through `reg.exe` and announced with `WM_SETTINGCHANGE`.
//! `reg.exe` is the same house style as `schtasks.exe` and `certutil.exe`: no
//! extra crate, and no shell, so `%VAR%` inside an existing `PATH` survives.

use std::path::Path;

#[cfg(windows)]
use anyhow::{Context, Result};

/// The registry value that holds the user's `PATH`.
const VALUE: &str = "Path";
const KEY: &str = r"HKCU\Environment";

/// Add the program directory once. Returns whether anything changed.
#[cfg(windows)]
pub fn install(program_dir: &Path) -> Result<bool> {
    let (current, kind) = read_user_path();
    let Some(next) = add_entry(&current, program_dir) else {
        return Ok(false);
    };
    write_user_path(&next, &kind)?;
    broadcast_change();
    Ok(true)
}

/// Remove the program directory. Returns whether anything changed.
#[cfg(windows)]
pub fn remove(program_dir: &Path) -> Result<bool> {
    let (current, kind) = read_user_path();
    let Some(next) = remove_entry(&current, program_dir) else {
        return Ok(false);
    };
    write_user_path(&next, &kind)?;
    broadcast_change();
    Ok(true)
}

/// Is the program directory on the user's `PATH`?
#[cfg(windows)]
pub fn contains(program_dir: &Path) -> bool {
    contains_entry(&read_user_path().0, program_dir)
}

/// The `PATH` after adding `program_dir`, or `None` when it is already there.
pub fn add_entry(current: &str, program_dir: &Path) -> Option<String> {
    if contains_entry(current, program_dir) {
        return None;
    }
    let entry = program_dir.to_string_lossy();
    let trimmed = current.trim().trim_end_matches(';');
    Some(if trimmed.is_empty() {
        entry.to_string()
    } else {
        format!("{trimmed};{entry}")
    })
}

/// The `PATH` after removing `program_dir`, or `None` when it is not there.
pub fn remove_entry(current: &str, program_dir: &Path) -> Option<String> {
    let wanted = normalize(program_dir);
    let all: Vec<&str> = current
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .collect();
    let kept: Vec<&str> = all
        .iter()
        .copied()
        .filter(|entry| normalize(Path::new(entry.trim())) != wanted)
        .collect();
    if kept.len() == all.len() {
        return None;
    }
    Some(kept.join(";"))
}

/// Does `current` already carry this directory? Windows compares paths without
/// regard to case or a trailing separator.
pub fn contains_entry(current: &str, program_dir: &Path) -> bool {
    let wanted = normalize(program_dir);
    current
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .any(|entry| normalize(Path::new(entry.trim())) == wanted)
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(|character| character == '\\' || character == '/')
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn read_user_path() -> (String, String) {
    let output = std::process::Command::new("reg.exe")
        .args(["query", KEY, "/v", VALUE])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            parse_query(&String::from_utf8_lossy(&output.stdout)).unwrap_or_default()
        }
        // A missing value is an empty PATH, not an error: the first install
        // creates it.
        _ => (String::new(), String::new()),
    }
}

#[cfg(windows)]
fn parse_query(output: &str) -> Option<(String, String)> {
    for line in output.lines() {
        let Some(index) = line.find("REG_") else {
            continue;
        };
        let rest = line[index..].trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let kind = parts.next()?.to_string();
        let value = parts.next().unwrap_or_default().trim().to_string();
        return Some((value, kind));
    }
    None
}

#[cfg(windows)]
fn write_user_path(value: &str, kind: &str) -> Result<()> {
    // `Path` is normally REG_EXPAND_SZ; keep whatever type the user already had.
    let kind = if kind.is_empty() {
        "REG_EXPAND_SZ"
    } else {
        kind
    };
    let output = std::process::Command::new("reg.exe")
        .args(["add", KEY, "/v", VALUE, "/t", kind, "/d", value, "/f"])
        .output()
        .context("running reg.exe add for the user PATH")?;
    if !output.status.success() {
        let detail = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::bail!("writing the user PATH failed: {}", detail.trim());
    }
    Ok(())
}

#[cfg(windows)]
fn broadcast_change() {
    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            hwnd: isize,
            message: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }

    const HWND_BROADCAST: isize = 0xFFFF;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;

    let mut result = 0usize;
    let environment = "Environment\0";
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            &mut result,
        );
    }
}

#[cfg(not(windows))]
mod unsupported {
    use std::path::Path;

    use anyhow::Result;

    pub fn install(_program_dir: &Path) -> Result<bool> {
        anyhow::bail!("the user PATH is managed on Windows only in v0")
    }

    pub fn remove(_program_dir: &Path) -> Result<bool> {
        anyhow::bail!("the user PATH is managed on Windows only in v0")
    }

    pub fn contains(_program_dir: &Path) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub use unsupported::contains;
#[cfg(not(windows))]
pub use unsupported::install;
#[cfg(not(windows))]
pub use unsupported::remove;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const DIR: &str = r"C:\Users\someone\AppData\Local\Programs\Agentify";

    #[test]
    fn adding_appends_once_and_is_idempotent() {
        let dir = Path::new(DIR);
        let first = add_entry("C:\\Windows", dir).expect("appended");
        assert_eq!(first, format!("C:\\Windows;{DIR}"));
        assert_eq!(
            add_entry(&first, dir),
            None,
            "the same directory is not added twice"
        );
        assert_eq!(add_entry("", dir).as_deref(), Some(DIR));
        let trailing = format!("C:\\Windows;{DIR}");
        assert_eq!(
            add_entry("C:\\Windows;", dir).as_deref(),
            Some(trailing.as_str())
        );
    }

    #[test]
    fn an_existing_entry_is_recognised_whatever_its_case_or_trailing_slash() {
        let dir = Path::new(DIR);
        assert!(contains_entry(
            &format!("C:\\Windows;{};C:\\Other", DIR.to_ascii_lowercase()),
            dir
        ));
        assert!(contains_entry(&format!("C:\\Windows;{DIR}\\\\"), dir));
        assert!(contains_entry(&format!("{DIR}\\", ), dir));
        assert!(!contains_entry("C:\\Windows;C:\\Other", dir));
    }

    #[test]
    fn removing_keeps_every_other_entry_and_reports_whether_it_changed() {
        let dir = Path::new(DIR);
        let current = format!("C:\\Windows;{DIR};C:\\Other");
        assert_eq!(
            remove_entry(&current, dir).as_deref(),
            Some("C:\\Windows;C:\\Other")
        );
        assert_eq!(remove_entry("C:\\Windows;C:\\Other", dir), None);
        assert_eq!(remove_entry(&format!("{DIR}"), dir).as_deref(), Some(""));
    }

    #[cfg(windows)]
    #[test]
    fn the_registry_query_is_parsed_without_losing_spaces_or_percent_signs() {
        let output = "\r\nHKEY_CURRENT_USER\\Environment\r\n    Path    REG_EXPAND_SZ    %USERPROFILE%\\bin;C:\\Program Files\\Agentify\r\n\r\n";
        assert_eq!(
            parse_query(output),
            Some((
                "%USERPROFILE%\\bin;C:\\Program Files\\Agentify".to_string(),
                "REG_EXPAND_SZ".to_string()
            ))
        );
        assert_eq!(parse_query("ERROR: not found\r\n"), None);
    }
}
