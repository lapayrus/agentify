//! Where the daemon keeps its files, and how to find it.

use std::path::PathBuf;

/// Directory name used under the per-user data location.
pub const APP_DIR_NAME: &str = "Agentify";

/// Environment variable that overrides the data root. Every Agentify process
/// resolves the data root the same way, so tests and scratch runs can point at
/// a temporary directory.
pub const HOME_ENV: &str = "AGENTIFY_HOME";

/// The active data root: `AGENTIFY_HOME` when set, otherwise the per-user
/// non-roaming application data directory.
pub fn resolve_data_root() -> PathBuf {
    match std::env::var_os(HOME_ENV) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => platform_data_dir()
            .map(|base| base.join(APP_DIR_NAME))
            .unwrap_or_else(|| std::env::temp_dir().join(APP_DIR_NAME)),
    }
}

#[cfg(windows)]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}
