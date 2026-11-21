//! Argon2id password hashing, in PHC string format, with explicit cost parameters.
//!
//! Hashes are produced and verified through the `argon2` crate's Argon2id implementation;
//! the PHC string is the only thing stored. Cost parameters always travel with the code: a
//! new hash uses the [`HashParams`] of the current policy, and verification reads the cost
//! parameters recorded in the stored hash and reports an upgrade need via
//! [`Verified::needs_rehash`].
use std::fmt;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::AuthError;

/// The PHC algorithm identifier this crate writes and accepts.
const ARGON2_ALGORITHM_ID: &str = "argon2id";

/// The Argon2 version written into every new hash, and the only version accepted as current.
const PHC_VERSION: u32 = Version::V0x13 as u32;

/// The byte length every hash this crate writes has.
const DEFAULT_HASH_LENGTH: usize = 32;

/// A password as it was typed, held in memory only as long as the check needs.
///
/// Pass it to [`hash_password`]/[`verify_password`] and to
/// [`check`](crate::policy::PasswordPolicy::check), then drop it: the memory is zeroized on
/// drop. Redacted in [`Debug`] as `Secret(***)`, no `Display`.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Password {
    text: String,
}

impl Password {
    /// Wraps a presented password.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
        }
    }

    /// The password bytes, for hashing and verifying only.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// The password text; the policy reads it for length, strength and context rules.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The number of Unicode scalar values in the password (policy length).
    #[must_use]
    pub fn character_count(&self) -> usize {
        self.text.chars().count()
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Argon2id cost parameters, validated by the `argon2` crate at construction, so
/// [`hash_password`] can be infallible.
#[derive(Clone, Debug)]
pub struct HashParams {
    inner: Params,
}

impl HashParams {
    /// The OWASP-recommended minimum: 19,456 KiB of memory, two passes, one lane. Use this
    /// (or stronger) for every stored password.
    pub fn new(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Result<Self, AuthError> {
        Ok(Self {
            inner: Params::new(m_cost_kib, t_cost, p_cost, Some(DEFAULT_HASH_LENGTH))
                .map_err(|_| AuthError::HashParamsInvalid)?,
        })
    }

    /// Cheap parameters for tests only: 32 KiB, one pass, one lane. Hashes produced with
    /// these are fine to verify against themselves but must never be stored by the platform.
    #[must_use]
    pub fn for_tests() -> Self {
        // The constants below are exact, so the constructor cannot fail here.
        match Params::new(32, 1, 1, Some(DEFAULT_HASH_LENGTH)) {
            Ok(inner) => Self { inner },
            Err(_) => Self {
                inner: Params::default(),
            },
        }
    }
}

impl Default for HashParams {
    /// The OWASP minimum: 19,456 KiB, two passes, one lane, a 32-byte digest.
    fn default() -> Self {
        // The constants below are exact, so the constructor cannot fail here.
        match Params::new(19_456, 2, 1, Some(DEFAULT_HASH_LENGTH)) {
            Ok(inner) => Self { inner },
            Err(_) => Self {
                inner: Params::default(),
            },
        }
    }
}

/// A stored Argon2id hash in PHC string format, produced by [`hash_password`].
///
/// The PHC string is secret material in spirit: an attacker who reads it can verify guesses
/// offline. It is therefore zeroized on drop and redacted in `Debug`, and no `Display`.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PasswordHash {
    phc: String,
}

impl PasswordHash {
    /// The PHC string, for storing in the database. Never log it; treat it like a secret.
    #[must_use]
    pub fn phc_string(&self) -> &str {
        &self.phc
    }

    /// Parses a stored PHC string, rejecting anything that is not a complete, well-formed
    /// argon2id hash. A hash from another Argon2 variant or with unusable parameters is a
    /// typed error, never a panic.
    pub fn parse(stored: &str) -> Result<Self, AuthError> {
        let parsed = argon2::password_hash::PasswordHash::new(stored)
            .map_err(|_| AuthError::HashMalformed)?;
        if parsed.algorithm.as_str() != ARGON2_ALGORITHM_ID {
            return Err(AuthError::HashMalformed);
        }
        Params::try_from(&parsed).map_err(|_| AuthError::HashMalformed)?;
        Ok(Self {
            phc: stored.to_owned(),
        })
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The outcome of a successful password check.
///
/// `needs_rehash` is true when the stored hash used different or weaker parameters than the
/// current [`HashParams`]; recompute [`hash_password`] with the current parameters and store
/// the new hash after a successful login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    /// Whether the stored hash should be recomputed with current parameters.
    pub needs_rehash: bool,
}

/// Hashes a password into a new PHC string with a fresh random salt.
///
/// Infallible by construction: [`HashParams`] are range-checked at construction, and the
/// CSPRNG salt generation cannot fail.
#[must_use]
pub fn hash_password(password: &Password, params: &HashParams) -> PasswordHash {
    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.inner.clone());
    match argon.hash_password(password.as_bytes(), &salt) {
        Ok(hashed) => PasswordHash {
            phc: hashed.to_string(),
        },
        // Unreachable: the parameters above are validated on construction.
        Err(_) => panic!("hash_password failed with validated parameters; this is a bug"),
    }
}

/// Checks a password against a stored Argon2id hash with a typed error for a mismatch, and
/// reports whether the stored parameters are weaker or different from the current ones, so
/// the caller can upgrade the hash (rehash) after a successful login.
///
/// Parameters are read from the stored hash, not from `params`; `params` is only the
/// reference for the upgrade check.
pub fn verify_password(
    password: &Password,
    stored: &PasswordHash,
    params: &HashParams,
) -> Result<Verified, AuthError> {
    let parsed = argon2::password_hash::PasswordHash::new(stored.phc_string())
        .map_err(|_| AuthError::HashMalformed)?;
    // Only ever write argon2id, so a stored hash under another variant is not ours to verify.
    if parsed.algorithm.as_str() != ARGON2_ALGORITHM_ID {
        return Err(AuthError::HashMalformed);
    }
    let stored_params = Params::try_from(&parsed).map_err(|_| AuthError::HashMalformed)?;
    // Verification always uses the parameters the stored hash was written with; the
    // reference params matter only for the upgrade check below.
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, stored_params.clone());
    if argon.verify_password(password.as_bytes(), &parsed).is_err() {
        return Err(AuthError::PasswordMismatch);
    }
    let needs_rehash = stored_params.m_cost() != params.inner.m_cost()
        || stored_params.t_cost() != params.inner.t_cost()
        || stored_params.p_cost() != params.inner.p_cost()
        || stored_params.output_len().unwrap_or(DEFAULT_HASH_LENGTH) != DEFAULT_HASH_LENGTH
        || parsed.version != Some(PHC_VERSION);
    Ok(Verified { needs_rehash })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::expect_ok;

    fn password(text: &str) -> Password {
        Password::new(text)
    }

    #[test]
    fn hash_then_verify_round_trip() {
        let params = HashParams::for_tests();
        let pw = password("correct horse battery staple");
        let stored = hash_password(&pw, &params);
        let verified = expect_ok(verify_password(&pw, &stored, &params), "verify");
        assert!(!verified.needs_rehash);
    }

    #[test]
    fn default_params_are_owasp_minimum_and_written_to_the_phc_string() {
        let stored = hash_password(
            &password("correct horse battery staple"),
            &HashParams::default(),
        );
        assert!(
            stored
                .phc_string()
                .starts_with("$argon2id$v=19$m=19456,t=2,p=1$")
        );
    }

    #[test]
    fn wrong_password_fails() {
        let params = HashParams::for_tests();
        let stored = hash_password(&password("correct horse battery staple"), &params);
        match verify_password(&password("incorrect horse"), &stored, &params) {
            Err(AuthError::PasswordMismatch) => {}
            other => panic!("expected PasswordMismatch, got {other:?}"),
        }
    }

    #[test]
    fn needs_rehash_after_parameter_change() {
        let params = HashParams::for_tests();
        let pw = password("correct horse battery staple");
        let stored = hash_password(&pw, &params);
        let verified = expect_ok(
            verify_password(&pw, &stored, &HashParams::default()),
            "verify",
        );
        assert!(verified.needs_rehash);
    }

    #[test]
    fn same_password_hashes_differ_by_salt() {
        let params = HashParams::for_tests();
        let first = hash_password(&password("correct horse battery staple"), &params);
        let second = hash_password(&password("correct horse battery staple"), &params);
        assert_ne!(first.phc_string(), second.phc_string());
    }

    #[test]
    fn phc_strings_round_trip_through_parse() {
        let params = HashParams::for_tests();
        let stored = hash_password(&password("correct horse battery staple"), &params);
        let reparsed = expect_ok(PasswordHash::parse(stored.phc_string()), "parse");
        assert_eq!(reparsed.phc_string(), stored.phc_string());
        let verified = expect_ok(
            verify_password(
                &password("correct horse battery staple"),
                &reparsed,
                &params,
            ),
            "verify",
        );
        assert!(!verified.needs_rehash);
    }

    #[test]
    fn malformed_stored_hashes_are_rejected() {
        for text in ["", "nope"] {
            match PasswordHash::parse(text) {
                Err(AuthError::HashMalformed) => {}
                other => panic!("expected HashMalformed, got {other:?}"),
            }
        }
        // A well-formed PHC string under another Argon2 variant is not ours to verify.
        let argon2d = "$argon2d$v=19$m=32,t=1,p=1$c2FsdGhvdXNl$kBXNKIr20rjaMV1fwKS4Egl7U2G9XKRR";
        match PasswordHash::parse(argon2d) {
            Err(AuthError::HashMalformed) => {}
            other => panic!("expected HashMalformed for argon2d, got {other:?}"),
        }
        match HashParams::new(0, 2, 1) {
            Err(AuthError::HashParamsInvalid) => {}
            other => panic!("expected HashParamsInvalid, got {other:?}"),
        }
    }

    #[test]
    fn debug_is_redacted() {
        let pw = password("correct horse battery staple");
        assert_eq!(format!("{pw:?}"), "Secret(***)");
        let stored = hash_password(&pw, &HashParams::for_tests());
        assert_eq!(format!("{stored:?}"), "Secret(***)");
    }
}
