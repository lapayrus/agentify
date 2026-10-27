//! Moving bytes in and out of a Project's distro, without a host mount.
//!
//! There is no shared directory in v0 — a Project's files are on ext4 inside its
//! distro and the manifest cannot ask for a mount (`mount` stays false) — so a
//! transfer rides the one channel that already exists: the launcher's own pipes.
//! `copyIn` writes the body to `wsl.exe`'s stdin and the command `cat > path`
//! puts it on the guest's disk; `copyOut` runs `cat path` and reads the bytes
//! back from stdout. Nothing is buffered on a host-visible path in between, and
//! no `\\wsl$\...` share is opened.
//!
//! Before a byte moves, both paths are checked by [`super::paths`]: a guest path
//! is absolute, POSIX, and never climbs; a host path is absolute, never climbs,
//! and must sit inside the daemon-owned root when the adapter has one. The check
//! happens first, so a traversal attempt is a typed refusal and not a runtime
//! error.
//!
//! Scope, stated plainly: v0 moves single regular files. A directory, a device,
//! or a symlink is refused rather than silently followed.

use std::path::Path;

use crate::error::{BackendError, Result};

use super::cli::Wsl;
use super::paths;

/// `mkdir -p <parent> && cat > <guest> && chmod 0644 <guest>`
pub fn copy_in_script(guest: &str) -> String {
    let quoted = paths::shell_quote(guest);
    let parent = parent_of(guest);
    format!(
        "mkdir -p {parent} && cat > {quoted} && chmod 0644 {quoted}",
        parent = paths::shell_quote(&parent)
    )
}

/// `cat <guest>`, with the guest path quoted so nothing in it is a command.
pub fn copy_out_script(guest: &str) -> String {
    format!("cat {}", paths::shell_quote(guest))
}

fn parent_of(guest: &str) -> String {
    match guest.rsplit_once('/') {
        Some(("", _)) | None => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
    }
}

/// The boundary check on its own: both paths, before anything is asked of the
/// runtime. `copy_in` and `copy_out` call it first, and so does the adapter, so a
/// traversal is refused even for an Environment that has no runtime yet.
pub fn check_paths(host: &Path, guest: &str, root: Option<&Path>) -> Result<()> {
    paths::host_path(host, root)?;
    paths::guest_path(guest)?;
    Ok(())
}

/// Copy one daemon-owned host file into the distro.
pub fn copy_in(
    wsl: &Wsl,
    distro: &str,
    host: &Path,
    guest: &str,
    root: Option<&Path>,
) -> Result<()> {
    let host = paths::host_path(host, root)?;
    let guest = paths::guest_path(guest)?;

    let metadata = std::fs::metadata(&host).map_err(|err| BackendError::ExecutionFailed {
        code: 1,
        detail: format!("cannot read {}: {err}", host.display()),
    })?;
    if !metadata.is_file() {
        return Err(BackendError::ExecutionFailed {
            code: 2,
            detail: format!(
                "{} is not a regular file; v0 copies files, not trees",
                host.display()
            ),
        });
    }
    let bytes = std::fs::read(&host).map_err(|err| BackendError::ExecutionFailed {
        code: 1,
        detail: format!("cannot read {}: {err}", host.display()),
    })?;

    let script = copy_in_script(&guest);
    let output = wsl.run_with_stdin(distro, &["sh", "-c", &script], &bytes)?;
    if output.code != 0 {
        return Err(BackendError::ExecutionFailed {
            code: output.code,
            detail: format!(
                "cannot write {guest} in {distro}: {}",
                output.diagnostic()
            ),
        });
    }
    Ok(())
}

/// Copy one file out of the distro into a daemon-owned host path.
pub fn copy_out(
    wsl: &Wsl,
    distro: &str,
    guest: &str,
    host: &Path,
    root: Option<&Path>,
) -> Result<()> {
    let host = paths::host_path(host, root)?;
    let guest = paths::guest_path(guest)?;

    // The destination directory has to be one the daemon owns and that already
    // exists: this call creates a file, not a tree.
    match host.parent() {
        Some(parent) if parent.is_dir() => {}
        Some(parent) => {
            return Err(BackendError::ExecutionFailed {
                code: 2,
                detail: format!(
                    "{} is not a directory the daemon owns; copyOut writes one file into it",
                    parent.display()
                ),
            })
        }
        None => {
            return Err(BackendError::ExecutionFailed {
                code: 2,
                detail: format!("{} has no parent directory", host.display()),
            })
        }
    }

    let output = wsl.run(distro, &["sh", "-c", &copy_out_script(&guest)])?;
    if output.code != 0 {
        return Err(BackendError::NotProvisioned {
            detail: format!(
                "no file at {guest} in {distro}: {}",
                output.diagnostic()
            ),
        });
    }
    std::fs::write(&host, &output.stdout).map_err(|err| BackendError::ExecutionFailed {
        code: 1,
        detail: format!("cannot write {}: {err}", host.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wsl() -> Wsl {
        Wsl::new("agentify-t8-deadbeef")
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentify-wsl-copy-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn the_copy_scripts_quote_the_guest_path() {
        assert_eq!(
            copy_in_script("/tmp/a.txt"),
            "mkdir -p '/tmp' && cat > '/tmp/a.txt' && chmod 0644 '/tmp/a.txt'"
        );
        assert_eq!(copy_in_script("/a.txt"), "mkdir -p '/' && cat > '/a.txt' && chmod 0644 '/a.txt'");
        assert_eq!(copy_out_script("/tmp/a.txt"), "cat '/tmp/a.txt'");
        assert_eq!(
            copy_out_script("/tmp/it's.txt"),
            "cat '/tmp/it'\\''s.txt'"
        );
    }

    #[test]
    fn a_traversal_is_refused_before_the_runtime_is_asked_anything() {
        let dir = scratch("traversal");
        let source = dir.join("in.txt");
        std::fs::write(&source, b"payload").expect("write");

        // Every one of these must fail as a *typed refusal*, not as "WSL is not
        // available here" — the check runs first, which is the point of doing it
        // at the boundary.
        for guest in ["/../etc/passwd", "etc/passwd", "/tmp/../../etc/shadow", "/"] {
            let err = copy_in(&wsl(), "nowhere", &source, guest, None)
                .expect_err("a refused guest path");
            assert_eq!(err.code(), "ExecutionFailed", "{guest}");
            assert!(err.to_string().contains("refused a transfer path"), "{err}");
        }

        let outside = if cfg!(windows) {
            PathBuf::from("C:/Windows/System32/config/SAM")
        } else {
            PathBuf::from("/etc/shadow")
        };
        let err = copy_out(&wsl(), "nowhere", "/tmp/x", &outside, None);
        assert!(
            err.is_err(),
            "a host path outside the daemon's world cannot be the target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_out_target_must_be_an_existing_directory() {
        let dir = scratch("target");
        let missing = dir.join("not-there").join("out.txt");
        let err = copy_out(&wsl(), "nowhere", "/tmp/x", &missing, None)
            .expect_err("the parent directory has to exist");
        assert_eq!(err.code(), "ExecutionFailed");
        assert!(err.to_string().contains("not a directory"), "{err}");

        // And a host path inside a configured root is accepted as far as the
        // runtime: with no WSL here, the failure is now about WSL, which proves
        // the path itself passed.
        let target = dir.join("out.txt");
        let err = copy_out(&wsl(), "nowhere", "/tmp/x", &target, None)
            .expect_err("no distro to read from");
        assert_ne!(
            err.to_string().contains("refused a transfer path"),
            true,
            "the path was accepted: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
