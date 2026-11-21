//! One-time enrolment codes.
//!
//! A code is `<enrolment id>.<secret>`: the id is a public lookup key, the secret is 16 random
//! bytes (hex) from the **injected** [`RandomSource`] (no clock-derived values, nothing read from
//! the environment here). Only a salted SHA-256 of the secret is stored, with a per-code salt;
//! the comparison is constant time; the code is never logged and never in an audit record (only
//! the enrolment id is).

use sha2::{Digest, Sha256};

use crate::error::{EnrolError, Result};

/// The secret's length in bytes.
pub const SECRET_BYTES: usize = 16;
/// The per-code salt's length in bytes.
pub const SALT_BYTES: usize = 16;

/// A source of random bytes: the caller's CSPRNG (or a fixed sequence in a test).
pub trait RandomSource: Send + Sync {
    /// Fills `buffer` with random bytes.
    fn fill(&self, buffer: &mut [u8]);
}

/// A freshly issued code: shown once to the administrator, never stored.
pub struct IssuedCode {
    /// The text to hand to the operator: `<id>.<secret hex>`.
    pub text: String,
    /// The public enrolment id inside the code.
    pub id: String,
    /// The salt to store.
    pub salt: [u8; SALT_BYTES],
    /// The salted hash to store.
    pub hash: [u8; 32],
}

impl std::fmt::Debug for IssuedCode {
    /// Never shows the code, the salt or the hash.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedCode")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// The salted hash of a secret.
#[must_use]
pub fn hash_secret(salt: &[u8], secret: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(secret);
    hasher.finalize().into()
}

/// Constant-time equality of two equally long byte strings (unequal lengths are unequal at once,
/// which reveals only the public length).
#[must_use]
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Whether `secret` is the secret behind `salt` and `hash`.
#[must_use]
pub fn verify_secret(salt: &[u8], hash: &[u8], secret: &[u8]) -> bool {
    constant_time_eq(&hash_secret(salt, secret), hash)
}

/// Issues a code for the enrolment `id` (a lower-case identifier chosen by the caller).
///
/// # Errors
/// `sync_enrol.invalid_argument` when the id is not 1 to 64 of `[a-z0-9_]`.
pub fn issue(id: &str, random: &dyn RandomSource) -> Result<IssuedCode> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(EnrolError::InvalidArgument {
            reason: "an enrolment id is 1 to 64 lower-case letters, digits or underscores",
        });
    }
    let mut secret = [0u8; SECRET_BYTES];
    random.fill(&mut secret);
    let mut salt = [0u8; SALT_BYTES];
    random.fill(&mut salt);
    Ok(IssuedCode {
        text: format!("{id}.{}", hex(&secret)),
        id: id.to_owned(),
        salt,
        hash: hash_secret(&salt, &secret),
    })
}

/// A code taken apart: the public id and the secret bytes.
pub struct ParsedCode {
    /// The enrolment id.
    pub id: String,
    /// The secret.
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for ParsedCode {
    /// Never shows the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedCode")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Splits a presented code. A malformed code is [`EnrolError::CodeInvalid`], the same error as an
/// unknown, expired or used one.
///
/// # Errors
/// `sync_enrol.code_invalid`.
pub fn parse(text: &str) -> Result<ParsedCode> {
    let (id, secret) = text.split_once('.').ok_or(EnrolError::CodeInvalid)?;
    let secret = unhex(secret).ok_or(EnrolError::CodeInvalid)?;
    if id.is_empty() || id.len() > 64 || secret.len() != SECRET_BYTES {
        return Err(EnrolError::CodeInvalid);
    }
    Ok(ParsedCode {
        id: id.to_owned(),
        secret,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    struct Counting(AtomicU8);
    impl RandomSource for Counting {
        fn fill(&self, buffer: &mut [u8]) {
            for byte in buffer {
                *byte = self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn an_issued_code_parses_and_verifies_once_hashed() {
        let issued = issue("enrol_1", &Counting(AtomicU8::new(1))).unwrap();
        let parsed = parse(&issued.text).unwrap();
        assert_eq!(parsed.id, "enrol_1");
        assert!(verify_secret(&issued.salt, &issued.hash, &parsed.secret));
        let mut wrong = parsed.secret.clone();
        wrong[0] ^= 1;
        assert!(!verify_secret(&issued.salt, &issued.hash, &wrong));
    }

    #[test]
    fn malformed_codes_are_all_the_same_error() {
        for text in [
            "",
            "nodot",
            "id.",
            "id.zz",
            "id.00",
            ".00112233445566778899aabbccddeeff",
            "a.b.c",
        ] {
            assert_eq!(parse(text).unwrap_err(), EnrolError::CodeInvalid, "{text}");
        }
    }

    #[test]
    fn the_debug_forms_show_no_secret() {
        let issued = issue("e", &Counting(AtomicU8::new(7))).unwrap();
        let shown = format!("{issued:?} {:?}", parse(&issued.text).unwrap());
        assert!(!shown.contains(&issued.text[2..]), "{shown}");
    }

    #[test]
    fn constant_time_eq_is_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
