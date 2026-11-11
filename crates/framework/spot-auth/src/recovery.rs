//! Recovery codes: single-use, printable fallbacks for when the authenticator is lost.
//!
//! Ten codes are generated at enrolment. The user sees each code once; the system stores
//! only SHA-256 digests, and when a login matches a digest the caller burns it by dropping
//! it from the stored list. Codes are high-entropy (32-symbol alphabet, 10 symbols), so a
//! fast SHA-256 digest is enough protection at rest.
use std::fmt;

use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::AuthError;
use crate::util::sha256;

/// The 32 characters recovery codes are drawn from, without the look-alikes `0 O 1 l I`.
pub const RECOVERY_ALPHABET: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// The number of characters in one recovery code; shown to users as `xxxxx-xxxxx`.
pub const RECOVERY_CODE_LENGTH: usize = 10;

/// How many codes [`generate_recovery_codes`] produces in the common case.
pub const DEFAULT_RECOVERY_CODE_COUNT: usize = 10;

// Generation masks an index byte to the alphabet size, so the alphabet must be exactly 32
// characters for the draws to be unbiased.
const _: () = assert!(RECOVERY_ALPHABET.len() == 32);

/// A recovery code, shown to the user exactly once and never printed back by the platform.
///
/// Redacted in [`Debug`] as `Secret(***)`, no `Display`, zeroized on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct RecoveryCode {
    code: String,
}

impl RecoveryCode {
    /// The display form, `xxxxx-xxxxx`. Treat it like a secret: never log it.
    #[must_use]
    pub fn display(&self) -> String {
        let (left, right) = self.code.split_at(RECOVERY_CODE_LENGTH / 2);
        format!("{left}-{right}")
    }

    /// The SHA-256 digest of this code: what the platform stores. The digest covers the
    /// normalised code text, so stored digests match case-, hyphen- and space-insensitive
    /// input in [`verify_recovery_code`].
    #[must_use]
    pub fn digest(&self) -> StoredDigest {
        StoredDigest {
            bytes: sha256(normalize(&self.code).as_bytes()),
        }
    }
}

impl fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Generates `count` fresh recovery codes from the OS CSPRNG.
///
/// Pass [`DEFAULT_RECOVERY_CODE_COUNT`] for the usual ten codes.
#[must_use]
pub fn generate_recovery_codes(count: usize) -> Vec<RecoveryCode> {
    (0..count)
        .map(|_| {
            let mut indexes = [0u8; RECOVERY_CODE_LENGTH];
            crate::util::fill_os_random(&mut indexes);
            let code: String = indexes
                .iter()
                // The alphabet has exactly 32 characters (checked above), so masking the byte
                // to 5 bits gives a uniform, unbiased index.
                .map(|index| RECOVERY_ALPHABET.as_bytes()[(index & 31) as usize] as char)
                .collect();
            RecoveryCode { code }
        })
        .collect()
}

/// A SHA-256 digest of one recovery code: the at-rest form stored by the platform.
///
/// It is secret material verified in a constant-time comparison, so it is redacted in
/// `Debug` and zeroized on drop like every other secret type in this crate.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct StoredDigest {
    bytes: [u8; 32],
}

impl StoredDigest {
    /// Builds a digest from exactly 32 previously stored bytes, for storage layers.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AuthError> {
        let array: [u8; 32] = bytes.try_into().map_err(|_| AuthError::DigestMalformed)?;
        Ok(Self { bytes: array })
    }

    /// The 32 bytes of the digest, for storage layers.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Compares against another digest in constant time.
    #[must_use]
    pub fn matches(&self, presented: &StoredDigest) -> bool {
        bool::from(self.bytes.as_slice().ct_eq(&presented.bytes))
    }
}

impl fmt::Debug for StoredDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Checks a typed recovery code against every stored digest and returns the index of the
/// one that matched, so the caller can burn that digest.
///
/// Input is normalised (case, spaces and hyphens are ignored) before hashing. Every stored
/// digest is compared with the presented one in constant time and with no early exit, so
/// timing reveals nothing about how many codes are valid or which one matched.
pub fn verify_recovery_code(input: &str, stored: &[StoredDigest]) -> Option<usize> {
    let normalized = normalize(input);
    let well_formed = normalized.chars().count() == RECOVERY_CODE_LENGTH
        && normalized.chars().all(|c| RECOVERY_ALPHABET.contains(c));
    if !well_formed {
        return None;
    }
    let presented = StoredDigest {
        bytes: sha256(normalized.as_bytes()),
    };
    let mut matched = None;
    for (index, digest) in stored.iter().enumerate() {
        if digest.matches(&presented) {
            // No early exit: the scan keeps going over every remaining digest, so timing does
            // not reveal which position matched. Duplicates cannot be stored anyway.
            matched = Some(index);
        }
    }
    matched
}

/// Uppercases the text and removes whitespace and hyphens.
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::expect_ok;

    #[test]
    fn codes_use_the_look_alike_free_alphabet_and_grouping() {
        let codes = generate_recovery_codes(DEFAULT_RECOVERY_CODE_COUNT);
        assert_eq!(codes.len(), DEFAULT_RECOVERY_CODE_COUNT);
        for code in &codes {
            let display = code.display();
            assert_eq!(display.len(), RECOVERY_CODE_LENGTH + 1);
            for (position, c) in display.chars().enumerate() {
                if position == RECOVERY_CODE_LENGTH / 2 {
                    assert_eq!(c, '-');
                } else {
                    assert!(RECOVERY_ALPHABET.contains(c), "{c} is not in the alphabet");
                }
            }
        }
    }

    #[test]
    fn stored_digests_reconstruct_and_match() {
        let codes = generate_recovery_codes(4);
        let digests: Vec<StoredDigest> = codes.iter().map(RecoveryCode::digest).collect();
        let original = codes[1].digest();
        let rebuilt = expect_ok(StoredDigest::from_bytes(original.as_bytes()), "digest");
        assert!(rebuilt.matches(&original));
        assert_eq!(verify_recovery_code(&codes[1].display(), &digests), Some(1));
        assert_eq!(verify_recovery_code(&codes[0].display(), &digests), Some(0));
    }

    #[test]
    fn reconstruction_rejects_wrong_lengths() {
        let codes = generate_recovery_codes(1);
        let original = codes[0].digest();
        match StoredDigest::from_bytes(&original.as_bytes()[..31]) {
            Err(AuthError::DigestMalformed) => {}
            other => panic!("expected DigestMalformed, got {other:?}"),
        }
    }

    #[test]
    fn normalization_ignores_case_spaces_and_hyphens() {
        let codes = generate_recovery_codes(4);
        let digests: Vec<StoredDigest> = codes.iter().map(RecoveryCode::digest).collect();
        let input = format!(" {} ", codes[2].display().to_lowercase());
        assert_eq!(verify_recovery_code(&input, &digests), Some(2));
    }

    #[test]
    fn wrong_codes_match_nothing() {
        let codes = generate_recovery_codes(4);
        let digests: Vec<StoredDigest> = codes.iter().map(RecoveryCode::digest).collect();
        assert_eq!(verify_recovery_code("XXXXX-XXXXX", &digests), None);
        assert_eq!(verify_recovery_code("", &digests), None);
        // Digits are not in the alphabet at all: the shape is rejected before hashing.
        assert_eq!(verify_recovery_code("12345-12345", &digests), None);
    }

    #[test]
    fn debug_is_redacted_and_display_is_absent() {
        let mut codes = generate_recovery_codes(1);
        let Some(code) = codes.pop() else {
            panic!("expected one code");
        };
        assert_eq!(format!("{code:?}"), "Secret(***)");
    }
}
