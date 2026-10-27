//! The path rules a transfer has to satisfy before a byte moves.
//!
//! `copyIn` and `copyOut` are the only calls that carry a path across the
//! boundary between the daemon's filesystem and a Project's, so the rules live
//! here, in one place, and are checked *before* anything is read, written, or
//! run:
//!
//! * a **guest path** is absolute, POSIX, and may not climb: a `..` component is
//!   refused outright rather than normalised away, because `a/../../b` is a
//!   traversal attempt however it works out;
//! * a **host path** is absolute, may not climb, and must sit inside the
//!   daemon-owned root when one is configured — the daemon is the only caller,
//!   and this is the adapter's side of that promise.
//!
//! Nothing here touches the filesystem beyond `Path` inspection, so both rules
//! are unit-tested everywhere, WSL or not.

use std::path::{Component, Path, PathBuf};

use crate::error::BackendError;

/// The typed failure a rejected path earns. `ExecutionFailed` is the contract's
/// "this command could not be run" slot, and a transfer with an unservable path
/// is exactly that: it never reaches the runtime.
fn rejected(detail: String) -> BackendError {
    BackendError::ExecutionFailed {
        code: 2,
        detail: format!("refused a transfer path: {detail}"),
    }
}

/// Normalise an absolute guest path, or explain why it cannot be served.
///
/// The result is the canonical spelling of the same path: `/a//b/` and `/a/b`
/// name one file, and a caller that gets `/a/b` back can quote it safely.
pub fn guest_path(raw: &str) -> crate::error::Result<String> {
    if raw.trim().is_empty() {
        return Err(rejected("a guest path cannot be empty".to_string()));
    }
    if raw.contains('\0') {
        return Err(rejected(
            "a guest path cannot contain a NUL byte".to_string(),
        ));
    }
    if raw.contains('\\') {
        return Err(rejected(format!(
            "{raw:?} uses backslashes; a guest path is a POSIX path"
        )));
    }
    if !raw.starts_with('/') {
        return Err(rejected(format!(
            "{raw:?} is relative; a guest path must be absolute"
        )));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(rejected(format!(
                    "{raw:?} climbs out of its root with `..`"
                )))
            }
            other => {
                if other.chars().any(char::is_control) {
                    return Err(rejected(format!(
                        "{raw:?} contains a control character"
                    )));
                }
                parts.push(other);
            }
        }
    }
    if parts.is_empty() {
        return Err(rejected(format!(
            "{raw:?} names the filesystem root; a transfer needs a file"
        )));
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Accept an absolute, non-climbing host path, and insist it is inside `root`
/// when the adapter has one.
pub fn host_path(path: &Path, root: Option<&Path>) -> crate::error::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(rejected("a host path cannot be empty".to_string()));
    }
    if !path.is_absolute() {
        return Err(rejected(format!(
            "{} is relative; host paths are daemon-owned and always absolute",
            path.display()
        )));
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(rejected(format!(
            "{} climbs upward with `..`",
            path.display()
        )));
    }
    if let Some(root) = root {
        if !root.is_absolute() {
            return Err(BackendError::BackendUnavailable {
                detail: format!(
                    "the configured host transfer root {} is not absolute",
                    root.display()
                ),
            });
        }
        if !path.starts_with(root) {
            return Err(rejected(format!(
                "{} is outside the daemon-owned root {}",
                path.display(),
                root.display()
            )));
        }
    }
    Ok(path.to_path_buf())
}

/// Quote one string for `/bin/sh`, so a guest path with a space or a quote is
/// still one argument and still not an injection.
pub fn shell_quote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for character in raw.chars() {
        if character == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(character);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guest_path_is_normalised_not_guessed() {
        assert_eq!(guest_path("/tmp/a.txt").unwrap(), "/tmp/a.txt");
        assert_eq!(guest_path("/tmp//a.txt").unwrap(), "/tmp/a.txt");
        assert_eq!(guest_path("/tmp/./a.txt").unwrap(), "/tmp/a.txt");
        assert_eq!(guest_path("/tmp/dir/").unwrap(), "/tmp/dir");
    }

    #[test]
    fn a_guest_path_that_climbs_is_refused() {
        for raw in ["/tmp/../../etc/passwd", "/..", "/a/../b", "/a/b/.."] {
            let err = guest_path(raw).expect_err(raw);
            assert_eq!(err.code(), "ExecutionFailed", "{raw}");
            assert!(
                err.to_string().contains("climbs out"),
                "the refusal says why: {err}"
            );
        }
    }

    #[test]
    fn a_guest_path_must_be_absolute_posix_and_a_file_below_root() {
        assert!(guest_path("tmp/a.txt").is_err(), "relative");
        assert!(guest_path("").is_err(), "empty");
        assert!(guest_path("   ").is_err(), "blank");
        assert!(guest_path("/").is_err(), "the root itself");
        assert!(guest_path("/tmp/a\0b").is_err(), "a NUL byte");
        assert!(guest_path("C:/tmp/a").is_err(), "a Windows path");
        assert!(guest_path("/tmp\\a").is_err(), "a backslash");
        assert!(guest_path("/tmp/a\nb").is_err(), "a control character");
    }

    #[test]
    fn a_host_path_must_be_absolute_and_must_not_climb() {
        let absolute = if cfg!(windows) {
            PathBuf::from("C:/data/projects/p/workspace/in.txt")
        } else {
            PathBuf::from("/data/projects/p/workspace/in.txt")
        };
        assert_eq!(host_path(&absolute, None).unwrap(), absolute);

        let relative = PathBuf::from("workspace/in.txt");
        assert_eq!(
            host_path(&relative, None).expect_err("relative").code(),
            "ExecutionFailed"
        );

        let climbing = absolute.join("..").join("..").join("secrets.txt");
        let err = host_path(&climbing, None).expect_err("climbs");
        assert!(err.to_string().contains("climbs upward"), "{err}");
    }

    #[test]
    fn a_host_path_outside_the_daemon_root_is_refused() {
        let root = if cfg!(windows) {
            PathBuf::from("C:/data")
        } else {
            PathBuf::from("/data")
        };
        let inside = root.join("projects").join("p").join("in.txt");
        assert!(host_path(&inside, Some(&root)).is_ok());

        let outside = if cfg!(windows) {
            PathBuf::from("C:/Windows/System32/config/SAM")
        } else {
            PathBuf::from("/etc/shadow")
        };
        let err = host_path(&outside, Some(&root)).expect_err("outside");
        assert!(
            err.to_string().contains("daemon-owned root"),
            "the refusal names the rule: {err}"
        );

        // A sibling that only shares a prefix is not inside the root.
        let sibling = if cfg!(windows) {
            PathBuf::from("C:/data-other/x")
        } else {
            PathBuf::from("/data-other/x")
        };
        assert!(host_path(&sibling, Some(&root)).is_err());
    }

    #[test]
    fn a_guest_path_is_quoted_for_the_shell() {
        assert_eq!(shell_quote("/tmp/a.txt"), "'/tmp/a.txt'");
        assert_eq!(
            shell_quote("/tmp/it's here.txt"),
            "'/tmp/it'\\''s here.txt'"
        );
        assert_eq!(shell_quote(""), "''");
    }
}
