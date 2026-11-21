//! One error for every credential failure, with a stable, machine-readable code per kind.
//!
//! Messages never contain secret material and never echo user input; the [`AuthError::code`]
//! values never change in a breaking way.

use std::fmt;

/// Something failed while handling credentials.
///
/// This is the one error type of this crate. A failed password check, a replayed one-time
/// code and a malformed stored hash are all distinct variants, so callers can act on them
/// without string matching; for logging and API responses use [`AuthError::code`].
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// A one-time code has the wrong length or contains characters other than digits.
    #[error("one-time code is the wrong length or contains non-digit characters")]
    TotpCodeInvalid,

    /// A one-time code matches a step at or before the step that was already used.
    #[error("one-time code was already used or is older than the last used code")]
    TotpReplayed,

    /// A binary secret was given in a length this crate cannot use.
    #[error("secret has an unusable length")]
    SecretLength,

    /// A stored password hash is not a complete PHC string or is not an argon2id hash.
    #[error("stored password hash is not a complete PHC string")]
    HashMalformed,

    /// Password hashing parameters are outside the accepted range.
    #[error("hash parameters are outside the accepted range")]
    HashParamsInvalid,

    /// A presented password does not match its stored hash.
    #[error("password does not match the stored hash")]
    PasswordMismatch,

    /// A presented API key does not have the expected shape; the input is never echoed.
    #[error("API key does not have the expected shape")]
    ApiKeyMalformed,

    /// A presented opaque token does not have the expected shape; the input is never echoed.
    #[error("opaque token does not have the expected shape")]
    TokenMalformed,

    /// A stored digest was given in a wrong length or not exactly 32 bytes.
    #[error("stored digest has an unusable shape")]
    DigestMalformed,

    /// Text claimed to be base32 is not valid RFC 4648 data.
    #[error("base32 text is not valid RFC 4648 data")]
    Base32Invalid,

    /// A cryptographic precondition that cannot occur in this design failed. This signals a
    /// bug in this crate, not bad input.
    #[error("cryptographic preconditions were not met; this is a bug")]
    CryptoInternal,
}

impl AuthError {
    /// The stable machine-readable code for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TotpCodeInvalid => "auth.totp.invalid",
            Self::TotpReplayed => "auth.totp.replayed",
            Self::SecretLength => "auth.secret.length",
            Self::HashMalformed => "auth.hash.malformed",
            Self::HashParamsInvalid => "auth.hash.params",
            Self::PasswordMismatch => "auth.password.mismatch",
            Self::ApiKeyMalformed => "auth.api_key.malformed",
            Self::TokenMalformed => "auth.token.malformed",
            Self::DigestMalformed => "auth.digest.malformed",
            Self::Base32Invalid => "auth.base32.invalid",
            Self::CryptoInternal => "auth.crypto.internal",
        }
    }
}

impl fmt::Debug for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Errors are never secret; Debug stays deliberate and short, echoing nothing.
        f.write_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_cover_every_variant() {
        let cases: Vec<(AuthError, &str)> = vec![
            (AuthError::TotpCodeInvalid, "auth.totp.invalid"),
            (AuthError::TotpReplayed, "auth.totp.replayed"),
            (AuthError::SecretLength, "auth.secret.length"),
            (AuthError::HashMalformed, "auth.hash.malformed"),
            (AuthError::HashParamsInvalid, "auth.hash.params"),
            (AuthError::PasswordMismatch, "auth.password.mismatch"),
            (AuthError::ApiKeyMalformed, "auth.api_key.malformed"),
            (AuthError::TokenMalformed, "auth.token.malformed"),
            (AuthError::DigestMalformed, "auth.digest.malformed"),
            (AuthError::Base32Invalid, "auth.base32.invalid"),
            (AuthError::CryptoInternal, "auth.crypto.internal"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }
}
