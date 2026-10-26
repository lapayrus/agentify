//! Agent token minting and hashing.
//!
//! Plaintext tokens never touch the database: only SHA-256 hashes are stored
//! (ADR-0003, "no plaintext tokens — token hashes only"). The one plaintext
//! copy lives in the user-only discovery file.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Bytes of entropy in an Agent token.
const TOKEN_BYTES: usize = 32;

/// Mint a fresh 256-bit token, hex-encoded.
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow::anyhow!("the OS random source failed: {err}"))
        .context("minting an Agent token")?;
    Ok(hex(&bytes))
}

/// SHA-256 of a token, hex-encoded. This is what the database stores.
pub fn sha256_hex(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex(&digest)
}

/// Compare two secrets without an early exit on the first differing byte.
pub fn secrets_match(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut diff = 0u8;
    for (lhs, rhs) in expected.bytes().zip(presented.bytes()) {
        diff |= lhs ^ rhs;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bits_of_hex_and_never_repeat() {
        let first = random_token().expect("token");
        let second = random_token().expect("token");
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }

    #[test]
    fn hashing_is_stable_and_compares_in_constant_time() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(secrets_match("abc", "abc"));
        assert!(!secrets_match("abc", "abd"));
        assert!(!secrets_match("abc", "ab"));
    }
}
