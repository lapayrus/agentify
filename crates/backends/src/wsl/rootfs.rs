//! The Ubuntu base rootfs: fetched once, verified against a pinned digest, and
//! cached under the data root.
//!
//! Nothing large is committed to the repository. A Project's distro is imported
//! from the cached tar, so a machine downloads it once and every later Project
//! reuses the file. The pinned digest is the whole point: a corrupted or
//! substituted download must fail before WSL ever sees it.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{BackendError, Result};

/// Ubuntu base 24.04.5, amd64. Pinned by name, digest, and size, because a
/// "latest" URL would silently change what a Project runs on.
pub const PINNED_ROOTFS_FILE: &str = "ubuntu-base-24.04.5-base-amd64.tar.gz";
pub const PINNED_ROOTFS_URL: &str =
    "https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.5-base-amd64.tar.gz";
/// SHA-256 of the file above, as published in the release's `SHA256SUMS`.
pub const PINNED_ROOTFS_SHA256: &str =
    "e77b6f10c2590cef872b33ee9f635a0e3fd1f57fb074c0e52b5c7f56147a0c86";
/// Pinned size in bytes, so a truncated download is caught before the hash pass.
pub const PINNED_ROOTFS_SIZE: u64 = 30_028_293;

/// Where a rootfs comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsSource {
    /// Download [`PINNED_ROOTFS_URL`] and verify [`PINNED_ROOTFS_SHA256`].
    Pinned,
    /// A tar already on disk (`AGENTIFY_WSL_ROOTFS`). Its digest is verified
    /// when one is given: tests use this to run without the network, and the
    /// production path never does.
    Local {
        path: PathBuf,
        sha256: Option<String>,
    },
}

impl RootfsSource {
    /// `AGENTIFY_WSL_ROOTFS` when set, the pinned download otherwise.
    pub fn from_override(local: Option<PathBuf>) -> Self {
        match local {
            Some(path) if !path.as_os_str().is_empty() => RootfsSource::Local {
                path,
                sha256: None,
            },
            _ => RootfsSource::Pinned,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            RootfsSource::Pinned => format!("{PINNED_ROOTFS_FILE} (pinned)"),
            RootfsSource::Local { path, .. } => format!("{} (local)", path.display()),
        }
    }
}

/// A verified rootfs on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rootfs {
    pub path: PathBuf,
    /// Lower-case hex SHA-256 actually measured for [`Rootfs::path`].
    pub sha256: String,
}

/// Make sure a verified rootfs tar exists under `cache_dir`, and return it.
///
/// A cache hit is re-verified rather than trusted: a half-written file from a
/// crashed run must not be imported into a distro.
pub fn ensure(source: &RootfsSource, cache_dir: &Path) -> Result<Rootfs> {
    match source {
        RootfsSource::Pinned => ensure_pinned(cache_dir),
        RootfsSource::Local { path, sha256 } => ensure_local(path, sha256.as_deref()),
    }
}

fn ensure_local(path: &Path, expected: Option<&str>) -> Result<Rootfs> {
    let metadata = std::fs::metadata(path).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("AGENTIFY_WSL_ROOTFS={} is not readable: {err}", path.display()),
    })?;
    if !metadata.is_file() {
        return Err(BackendError::BackendUnavailable {
            detail: format!("AGENTIFY_WSL_ROOTFS={} is not a file", path.display()),
        });
    }
    let sha256 = hash_file(path)?;
    if let Some(expected) = expected {
        if !sha256.eq_ignore_ascii_case(expected.trim()) {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "the rootfs at {} has SHA-256 {sha256}, not the pinned {expected}",
                    path.display()
                ),
            });
        }
    }
    Ok(Rootfs {
        path: path.to_path_buf(),
        sha256,
    })
}

fn ensure_pinned(cache_dir: &Path) -> Result<Rootfs> {
    std::fs::create_dir_all(cache_dir).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot create the rootfs cache {}: {err}", cache_dir.display()),
    })?;
    let path = cache_dir.join(PINNED_ROOTFS_FILE);
    if path.exists() {
        match hash_file(&path) {
            Ok(sha256) if sha256 == PINNED_ROOTFS_SHA256 => {
                // A marker records which digest the cached file satisfied, so a
                // future pin change invalidates the cache visibly.
                write_marker(&path, &sha256);
                return Ok(Rootfs { path, sha256 });
            }
            _ => {
                // A corrupt cache is deleted, not repaired: the download is the
                // only source of truth.
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let sha256 = download(&path)?;
    if sha256 != PINNED_ROOTFS_SHA256 {
        let _ = std::fs::remove_file(&path);
        return Err(BackendError::BackendUnavailable {
            detail: format!(
                "the rootfs download has SHA-256 {sha256}, not the pinned {PINNED_ROOTFS_SHA256}; refused"
            ),
        });
    }
    write_marker(&path, &sha256);
    Ok(Rootfs { path, sha256 })
}

/// Download to a temporary sibling, hashing as it goes, and rename into place.
/// A crash therefore leaves either the old file or nothing — never a trailer.
fn download(destination: &Path) -> Result<String> {
    let temporary = destination.with_extension("part");
    let response = ureq::get(PINNED_ROOTFS_URL)
        .call()
        .map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot download {PINNED_ROOTFS_URL}: {err}"),
        })?;
    let mut reader = response.into_reader();
    let mut file = File::create(&temporary).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot write {}: {err}", temporary.display()),
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| BackendError::BackendUnavailable {
                detail: format!("the rootfs download failed: {err}"),
            })?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|err| BackendError::BackendUnavailable {
                detail: format!("cannot write {}: {err}", temporary.display()),
            })?;
    }
    file.flush().ok();
    drop(file);

    let sha256 = hex(&hasher.finalize());
    if total != PINNED_ROOTFS_SIZE {
        let _ = std::fs::remove_file(&temporary);
        return Err(BackendError::BackendUnavailable {
            detail: format!(
                "the rootfs download is {total} bytes, not the pinned {PINNED_ROOTFS_SIZE}"
            ),
        });
    }
    std::fs::rename(&temporary, destination).map_err(|err| {
        BackendError::BackendUnavailable {
            detail: format!("cannot move the rootfs into the cache: {err}"),
        }
    })?;
    Ok(sha256)
}

/// SHA-256 of a file, lower-case hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot read {}: {err}", path.display()),
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| BackendError::BackendUnavailable {
                detail: format!("cannot read {}: {err}", path.display()),
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn write_marker(file: &Path, sha256: &str) {
    let marker = file.with_extension("sha256");
    let _ = std::fs::write(marker, format!("{sha256}\n"));
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_rootfs_with_the_wrong_digest_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "agentify-rootfs-{}",
            std::process::id() as u64 * 31 + 7
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let tar = dir.join("rootfs.tar");
        std::fs::write(&tar, b"not the rootfs you are looking for").expect("write tar");

        let source = RootfsSource::Local {
            path: tar.clone(),
            sha256: Some(PINNED_ROOTFS_SHA256.to_string()),
        };
        let err = ensure(&source, &dir).expect_err("a wrong digest must be refused");
        assert_eq!(err.code(), "BackendUnavailable");
        assert!(
            err.to_string().contains("SHA-256"),
            "the refusal names the digest: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_rootfs_reports_the_digest_it_measured() {
        let dir = std::env::temp_dir().join(format!(
            "agentify-rootfs-ok-{}",
            std::process::id() as u64 * 31 + 11
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let tar = dir.join("rootfs.tar");
        std::fs::write(&tar, b"payload").expect("write tar");

        // SHA-256 of "payload", computed independently of this crate.
        let source = RootfsSource::Local {
            path: tar,
            sha256: Some("239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5".to_string()),
        };
        let rootfs = ensure(&source, &dir).expect("a matching digest is accepted");
        assert_eq!(rootfs.sha256, "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_local_rootfs_names_the_variable() {
        let source = RootfsSource::Local {
            path: PathBuf::from("C:/definitely/not/here/rootfs.tar"),
            sha256: None,
        };
        let err = ensure(&source, &std::env::temp_dir()).expect_err("a missing tar fails");
        assert!(err.to_string().contains("AGENTIFY_WSL_ROOTFS"), "{err}");
    }

    #[test]
    fn an_override_only_wins_when_it_is_set() {
        assert_eq!(RootfsSource::from_override(None), RootfsSource::Pinned);
        assert_eq!(
            RootfsSource::from_override(Some(PathBuf::new())),
            RootfsSource::Pinned
        );
        assert!(matches!(
            RootfsSource::from_override(Some(PathBuf::from("C:/tar"))),
            RootfsSource::Local { .. }
        ));
    }
}
