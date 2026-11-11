//! Small JOSE helpers: unpadded Base64url, SHA-256 and constant-time comparison.
//!
//! Everything here is pure and takes its input as data; nothing reads the clock, the network
//! or any ambient state.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::digest::{SHA256, digest};

use crate::error::{self as e, Result, SsoError};

/// Unpadded Base64url of `bytes` (RFC 4648 §5), the JOSE encoding.
pub(crate) fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes unpadded Base64url `text`, refusing anything outside the alphabet or any padding.
pub(crate) fn decode(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .map_err(|_| SsoError::new(e::codes::TOKEN_MALFORMED, PUBLIC, ADMIN))
}

/// The SHA-256 digest of `data`.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, data).as_ref());
    out
}

/// Constant-time equality of two byte slices; `false` for differing lengths.
///
/// The comparison accumulates the XOR of every byte pair with no early exit, so it does not
/// stop at the first differing byte. `ring` exposes no stable constant-time compare, and this
/// crate may not add `subtle`, so this is a deliberate, minimal implementation; the lengths of
/// the values compared here (`state`, `nonce`, hashes) are fixed and not secret.
#[must_use]
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b) {
        difference |= left ^ right;
    }
    difference == 0
}

const PUBLIC: &str = "The sign-in response could not be read.";
const ADMIN: &str = "Check the identity provider's token format and try again.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::codes;

    #[test]
    fn base64url_round_trips_without_padding() {
        let bytes = [0u8, 1, 2, 250, 251, 255];
        let text = encode(&bytes);
        assert!(!text.contains(['+', '/', '=']));
        assert_eq!(decode(&text).unwrap_or_default(), bytes);
    }

    #[test]
    fn decode_refuses_padding_and_aliens() {
        for bad in ["AA==", "A+B/", "*"] {
            let error = match decode(bad) {
                Err(error) => error,
                Ok(_) => panic!("{bad:?} must not decode"),
            };
            assert_eq!(error.code(), codes::TOKEN_MALFORMED);
        }
    }

    #[test]
    fn constant_time_equality_matches() {
        assert!(ct_eq(b"abcd", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abce"));
        assert!(!ct_eq(b"abcd", b"abc"));
    }
}
