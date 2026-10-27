//! Free-space measurement for the `whoami` disk-pressure warning.
//!
//! This is a warning, not a gate: nothing here refuses a call, it only changes
//! what `whoami` reports. It is a seam so a test can script pressure without
//! touching the real volume.

use std::path::Path;

use anyhow::{Context, Result};

/// Below this many free bytes the daemon reports pressure.
pub const DISK_PRESSURE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Measures the free space on the volume that holds `path`.
pub trait DiskUsage: Send + Sync + std::fmt::Debug {
    fn free_bytes(&self, path: &Path) -> Result<u64>;
}

/// The real measurement. Windows is the shipped target; elsewhere there is no
/// cheap, dependency-free API here, so it reports "no pressure" rather than a
/// made-up number.
#[derive(Debug)]
pub struct SystemDiskUsage;

impl DiskUsage for SystemDiskUsage {
    fn free_bytes(&self, path: &Path) -> Result<u64> {
        #[cfg(windows)]
        {
            windows_free_bytes(path)
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Ok(u64::MAX)
        }
    }
}

/// Whether this many free bytes counts as pressure.
pub fn warning(free_bytes: u64) -> bool {
    free_bytes < DISK_PRESSURE_BYTES
}

#[cfg(windows)]
fn windows_free_bytes(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("measuring free space on {}", path.display()));
    }
    Ok(free_to_caller)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_is_below_the_threshold() {
        assert!(warning(DISK_PRESSURE_BYTES - 1));
        assert!(!warning(DISK_PRESSURE_BYTES));
        assert!(!warning(DISK_PRESSURE_BYTES + 1));
    }
}
