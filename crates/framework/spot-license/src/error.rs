//! Typed errors for `spot-license`: one enum per boundary, stable `licence.*` codes.
//!
//! Callers match on [`LicenceError::code`], never on the message: the codes are this
//! crate's API contract and never change in a breaking way. Messages are plain language,
//! never contain key material, and never echo untrusted input beyond what serde already
//! quotes in its parse diagnostics.

use std::fmt;

/// Something failed while parsing, validating or verifying a licence.
///
/// This is the one error type for the payload, the text form, the signature and the
/// trusted-key list. The lifecycle's refusals are **not** errors: `evaluate` returns a
/// [`Status`](crate::state::Status), and its refusal reasons live on
/// [`Decision`](crate::state::Decision), not here.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum LicenceError {
    /// The payload text or bytes are not parseable as a `Licence` document.
    #[error("not a valid licence payload: {message}")]
    Json {
        /// The serde error's message (already names the offending key or syntax).
        message: String,
    },

    /// A verified licence could not be serialised; for payloads this crate produced
    /// itself this means a bug here, not bad input.
    #[error("licence serialization failed: {message}")]
    Serialization {
        /// The serde error's message.
        message: String,
    },

    /// The payload's `version` field is not the version this crate mints.
    #[error("unknown licence format version {version}")]
    UnknownVersion {
        /// The rejected version number.
        version: u32,
    },

    /// The payload parses but is not in canonical form (field order, whitespace,
    /// duplicate keys or number spellings differ from what this crate emits).
    #[error("licence payload is not in canonical form")]
    NotCanonical,

    /// The `licence_id` is missing or longer than 128 bytes.
    #[error("licence id is missing or out of the accepted length range")]
    LicenceIdInvalid,

    /// The licence names neither or both of `tenant` and `installation_id`; exactly one
    /// holder is required.
    #[error("a licence names exactly one of tenant or installation_id")]
    HolderInvalid,

    /// The edition string is missing, too long or has characters outside the alphabet.
    #[error("edition is not in the accepted shape")]
    EditionInvalid,

    /// The licence's `issuer_key_id` is missing or out of the accepted length range.
    #[error("issuer key id is missing or out of the accepted length range")]
    KeyIdInvalid,

    /// The serial is 0; serials start at 1 and strictly increase per licence id.
    #[error("licence serial is 0; serials start at 1")]
    SerialZero,

    /// An instant in the licence is negative.
    #[error("an instant in the licence is negative")]
    InstantRange,

    /// The validity window is inconsistent: it must satisfy
    /// `not_before <= renew_after <= expires_at` for the fields that are set.
    #[error(
        "the validity window is inconsistent: not_before <= renew_after <= expires_at must hold"
    )]
    WindowInvalid,

    /// The grace period exceeds [`crate::MAX_GRACE`].
    #[error("grace period exceeds the maximum of {maximum_days} days")]
    GraceTooLong {
        /// The cap the licence exceeded, in whole days.
        maximum_days: u32,
    },

    /// The `kind` contradicts the rest of the licence: a trial needs an expiry and no
    /// grace, a perpetual has no expiry.
    #[error("licence kind contradicts the validity fields")]
    KindInvalid,

    /// The licence file text is not `SPOT-LICENCE-1.<base64url payload>.<base64url
    /// signature>` for well-formed values of the two parts.
    #[error("licence file is not in the SPOT-LICENCE-1 text form")]
    EnvelopeMalformed,

    /// The signature is present but not exactly 64 bytes.
    #[error("licence signature has an unusable length")]
    SignatureMalformed,

    /// The signature does not verify against any trusted key over the payload.
    #[error("licence signature is invalid")]
    SignatureInvalid,

    /// The envelope's `key_id` and the payload's `issuer_key_id` disagree.
    #[error("the envelope key id and the payload's issuer key id disagree")]
    KeyMismatch,

    /// The payload's issuer key id is not in the trusted key list. A key that was
    /// removed from the list (revocation) also lands here.
    #[error("licence is signed by an unknown issuer key")]
    KeyUnknown,

    /// The issuer key is known but its validity window does not cover the licence's
    /// `issued_at`: the licence was minted outside the key's trusted lifetime.
    #[error("licence was issued outside the signing key's validity window")]
    KeyExpired,

    /// A trusted key was built from bytes that are not a valid Ed25519 public key.
    #[error("trusted key bytes are not a valid Ed25519 public key")]
    KeyBytesInvalid,
}

impl LicenceError {
    /// The stable machine-readable code for logs, API responses and metrics.
    ///
    /// Codes are `licence.<snake_case>` and never change in a breaking way.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Json { .. } => "licence.json",
            Self::Serialization { .. } => "licence.serialization",
            Self::UnknownVersion { .. } => "licence.unknown_version",
            Self::NotCanonical => "licence.not_canonical",
            Self::LicenceIdInvalid => "licence.licence_id_invalid",
            Self::HolderInvalid => "licence.holder_invalid",
            Self::EditionInvalid => "licence.edition_invalid",
            Self::KeyIdInvalid => "licence.key_id_invalid",
            Self::SerialZero => "licence.serial_zero",
            Self::InstantRange => "licence.instant_range",
            Self::WindowInvalid => "licence.window_invalid",
            Self::GraceTooLong { .. } => "licence.grace_too_long",
            Self::KindInvalid => "licence.kind_invalid",
            Self::EnvelopeMalformed => "licence.envelope_malformed",
            Self::SignatureMalformed => "licence.signature_malformed",
            Self::SignatureInvalid => "licence.signature_invalid",
            Self::KeyMismatch => "licence.key_mismatch",
            Self::KeyUnknown => "licence.key_unknown",
            Self::KeyExpired => "licence.key_expired",
            Self::KeyBytesInvalid => "licence.key_bytes_invalid",
        }
    }
}

impl fmt::Debug for LicenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Errors are never secret; Debug stays deliberate and short, echoing nothing new.
        f.write_str(&self.to_string())
    }
}

/// Something failed while applying a licence renewal.
///
/// Distinct from [`LicenceError`]: renewals fail structurally (a stale serial, an
/// unrelated tenant) even when both licences are individually valid.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenewalError {
    /// The incoming licence's serial is not strictly higher than the current one; a
    /// replay of an older or equal licence.
    #[error("renewal serial {incoming} is not higher than the installed {current}")]
    Stale {
        /// The installed licence's serial.
        current: u64,
        /// The incoming licence's serial.
        incoming: u64,
    },

    /// The renewal carries a different tenant or installation id than the licence it
    /// would replace.
    #[error("renewal belongs to a different tenant or installation")]
    WrongTenant,

    /// The renewal is for a different licence id; it is a new grant, not a renewal.
    #[error("renewal is for a different licence id")]
    IdMismatch,

    /// The incoming licence itself is malformed; nothing was applied.
    #[error("renewal licence is invalid")]
    Invalid(#[from] LicenceError),
}

impl RenewalError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Stale { .. } => "licence.stale",
            Self::WrongTenant => "licence.wrong_tenant",
            Self::IdMismatch => "licence.licence_id_mismatch",
            Self::Invalid(_) => "licence.renewal_invalid",
        }
    }
}

impl fmt::Debug for RenewalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{LicenceError, RenewalError};

    #[test]
    fn licence_codes_are_stable_and_cover_every_variant() {
        let cases: Vec<(LicenceError, &str)> = vec![
            (
                LicenceError::Json {
                    message: "x".to_owned(),
                },
                "licence.json",
            ),
            (
                LicenceError::Serialization {
                    message: "x".to_owned(),
                },
                "licence.serialization",
            ),
            (
                LicenceError::UnknownVersion { version: 9 },
                "licence.unknown_version",
            ),
            (LicenceError::NotCanonical, "licence.not_canonical"),
            (LicenceError::LicenceIdInvalid, "licence.licence_id_invalid"),
            (LicenceError::HolderInvalid, "licence.holder_invalid"),
            (LicenceError::EditionInvalid, "licence.edition_invalid"),
            (LicenceError::KeyIdInvalid, "licence.key_id_invalid"),
            (LicenceError::SerialZero, "licence.serial_zero"),
            (LicenceError::InstantRange, "licence.instant_range"),
            (LicenceError::WindowInvalid, "licence.window_invalid"),
            (
                LicenceError::GraceTooLong { maximum_days: 90 },
                "licence.grace_too_long",
            ),
            (LicenceError::KindInvalid, "licence.kind_invalid"),
            (
                LicenceError::EnvelopeMalformed,
                "licence.envelope_malformed",
            ),
            (
                LicenceError::SignatureMalformed,
                "licence.signature_malformed",
            ),
            (LicenceError::SignatureInvalid, "licence.signature_invalid"),
            (LicenceError::KeyMismatch, "licence.key_mismatch"),
            (LicenceError::KeyUnknown, "licence.key_unknown"),
            (LicenceError::KeyExpired, "licence.key_expired"),
            (LicenceError::KeyBytesInvalid, "licence.key_bytes_invalid"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }

    #[test]
    fn renewal_codes_are_stable_and_cover_every_variant() {
        let cases: Vec<(RenewalError, &str)> = vec![
            (
                RenewalError::Stale {
                    current: 1,
                    incoming: 1,
                },
                "licence.stale",
            ),
            (RenewalError::WrongTenant, "licence.wrong_tenant"),
            (RenewalError::IdMismatch, "licence.licence_id_mismatch"),
            (
                RenewalError::Invalid(LicenceError::SerialZero),
                "licence.renewal_invalid",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }
}
