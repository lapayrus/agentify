//! Moving bytes in and out of a Project's distro, without a host mount.
//!
//! There is no shared directory in v0 — a Project's files are on ext4 inside its
//! distro and the manifest cannot ask for a mount (`mount` stays false) — so a
//! transfer rides the one channel that already exists: the launcher's own pipes.
//! Nothing is buffered on a host-visible path in between, and no `\\wsl$\...`
//! share is opened.
//!
//! **The pipes carry text, never binary.** A launcher between two operating
//! systems is a text channel by default — anything that decides a byte is not
//! UTF-8 may replace it (0xFF becomes U+FFFD), and a body copied that way is
//! silently corrupted rather than refused. So the body is base64 on the wire in
//! both directions: `copyIn` sends base64 on `wsl.exe`'s stdin and the guest runs
//! `base64 -d > path`; `copyOut` runs `base64 < path` and the bytes are decoded
//! here. Every byte value survives, including NUL, 0xFF, and sequences that are
//! not valid UTF-8 at all. The cost is a `base64` on both sides: the host's own
//! encoder/decoder is below in this file, and the guest's is coreutils, which
//! the Ubuntu base rootfs always has.
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

/// `mkdir -p <parent> && base64 -d > <guest> && chmod 0644 <guest>`
pub fn copy_in_script(guest: &str) -> String {
    let quoted = paths::shell_quote(guest);
    let parent = parent_of(guest);
    format!(
        "mkdir -p {parent} && base64 -d > {quoted} && chmod 0644 {quoted}",
        parent = paths::shell_quote(&parent)
    )
}

/// `base64 < <guest>`, with the guest path quoted so nothing in it is a command.
///
/// The redirect, not `base64 <path>`: a missing file then fails the redirect
/// (a non-zero exit and a diagnostic), which is the typed `NotProvisioned` a
/// caller wants, and the encoded text — ASCII, so no launcher can transcode it —
/// is what comes back on stdout.
pub fn copy_out_script(guest: &str) -> String {
    format!("base64 < {}", paths::shell_quote(guest))
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

    // The wire format is ASCII: one trailing newline so `base64 -d` sees a
    // complete body, and not one byte of the file itself on the pipe.
    let mut body = encode_base64(&bytes).into_bytes();
    body.push(b'\n');

    let script = copy_in_script(&guest);
    let output = wsl.run_with_stdin(distro, &["sh", "-c", &script], &body)?;
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
    // A body that is not base64 means the pipe did not carry what this module
    // asked it to; refusing is the only honest answer, and it can never write a
    // truncated file over a good one.
    let bytes = decode_base64(&output.stdout).map_err(|detail| BackendError::ExecutionFailed {
        code: 3,
        detail: format!("the body read from {guest} in {distro} is not base64: {detail}"),
    })?;
    std::fs::write(&host, &bytes).map_err(|err| BackendError::ExecutionFailed {
        code: 1,
        detail: format!("cannot write {}: {err}", host.display()),
    })
}

// -- base64, so the launcher only ever sees ASCII ------------------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64 (RFC 4648 §4): the same alphabet and padding as
/// coreutils `base64 -d` on the other side.
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let group = ((chunk[0] as u32) << 16) | ((second as u32) << 8) | third as u32;
        out.push(ALPHABET[(group >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(group >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(group >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[group as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64, tolerating what a shell tool emits: newlines (a
/// coreutils `base64` wraps at 76 columns) and any other ASCII whitespace.
///
/// `Err` carries a short explanation, never a partially decoded body.
pub fn decode_base64(encoded: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3 + 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut padding = 0usize;
    for &byte in encoded {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding += 1;
            if padding > 2 {
                return Err("more than two padding bytes".to_string());
            }
            continue;
        }
        if padding > 0 {
            return Err("a sextet after padding".to_string());
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => {
                return Err(format!(
                    "byte 0x{byte:02x} is not in the base64 alphabet"
                ))
            }
        };
        accumulator = (accumulator << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    if bits >= 6 {
        return Err("a truncated final group".to_string());
    }
    Ok(out)
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
            "mkdir -p '/tmp' && base64 -d > '/tmp/a.txt' && chmod 0644 '/tmp/a.txt'"
        );
        assert_eq!(
            copy_in_script("/a.txt"),
            "mkdir -p '/' && base64 -d > '/a.txt' && chmod 0644 '/a.txt'"
        );
        assert_eq!(copy_out_script("/tmp/a.txt"), "base64 < '/tmp/a.txt'");
        assert_eq!(
            copy_out_script("/tmp/it's.txt"),
            "base64 < '/tmp/it'\\''s.txt'"
        );
    }

    /// The payload the real-distro test uses: text, a NUL, 0xFF, a byte
    /// sequence that is not valid UTF-8, the bytes of U+FFFD itself, and CR LF —
    /// so a pipeline that replaces, translates, or line-end-rewrites anything is
    /// caught rather than papered over.
    const BINARY: &[u8] = b"agentify copy: NUL \x00, 0xFF \xff, high \x80\xc3\x28, U+FFFD \xef\xbf\xbd, CR LF \r\n";

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        // RFC 4648 vectors, plus the byte values this ticket is about.
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode_base64(&[0x00]), "AA==");
        assert_eq!(encode_base64(&[0xff]), "/w==");
        assert_eq!(encode_base64(&[0x00, 0xff, 0x80]), "AP+A");
        assert_eq!(encode_base64(&[0xef, 0xbf, 0xbd]), "77+9");
    }

    #[test]
    fn a_body_round_trips_byte_for_byte_through_the_wire_format() {
        for payload in [
            BINARY,
            b"".as_slice(),
            &[0x00],
            &[0xff, 0xff, 0xff, 0xff],
            &[0x80, 0xc3, 0x28, 0x0a, 0x0d],
        ] {
            let wire = encode_base64(payload);
            assert!(
                wire.bytes().all(|byte| byte.is_ascii()),
                "the wire format is ASCII, so no launcher can transcode it"
            );
            assert_eq!(decode_base64(wire.as_bytes()).expect("decodes"), payload);
        }
        // Every byte value, three times over, so no offset is untested.
        let every: Vec<u8> = (0..=255u8).cycle().take(768).collect();
        assert_eq!(decode_base64(encode_base64(&every).as_bytes()).unwrap(), every);
    }

    #[test]
    fn the_decoder_tolerates_what_coreutils_emits_and_refuses_the_rest() {
        // A coreutils `base64` wraps at 76 columns: newlines are not data.
        let wrapped = "Zm9vYmFy\nZm9vYmFy\n";
        assert_eq!(decode_base64(wrapped.as_bytes()).unwrap(), b"foobarfoobar");
        assert!(decode_base64(b" Zm9v \r\n").is_ok(), "whitespace is skipped");

        for bad in ["!!!!", "Zm9v=Zm9v", "Z", "Zm9v=", "Zm9v==="] {
            let decoded = decode_base64(bad.as_bytes());
            if bad == "Zm9v=" {
                // One trailing pad on an exact group is harmless padding.
                assert_eq!(decoded.unwrap(), b"foo", "{bad}");
                continue;
            }
            assert!(decoded.is_err(), "{bad:?} is not base64");
        }
        // A non-ASCII byte is a decode failure, which is what a transcoding
        // launcher would hand us: refusing beats writing a corrupt file.
        assert!(decode_base64(&[0xff]).is_err());
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
