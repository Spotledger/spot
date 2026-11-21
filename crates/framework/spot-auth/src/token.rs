//! Opaque bearer tokens and API keys.
//!
//! An [`OpaqueToken`] is the text a client presents in an `Authorization` header. At
//! generation time its SHA-256 digest is the only thing shown to the caller for storage;
//! comparing a presented token against a stored digest then leaks nothing about the token
//! even from a database dump, because the raw text lived only in [`OpaqueToken`].
//!
//! API keys live in the [`crate::api_key`] module. They re-use [`TokenDigest`], which is
//! why it lives here.
use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::AuthError;
use crate::util::sha256;

/// A bearer token the user never types: 43 un-padded Base64url characters for 32 bytes of
/// randomness, exactly as [`OpaqueToken::generate`] produces.
pub const OPAQUE_TOKEN_LENGTH: usize = 43;

const TOKEN_SECRET_BYTES: usize = 32;

/// A bearer token, held only as long as it takes to verify it.
///
/// Redacted in [`Debug`] as `Secret(***)`, no `Display`, zeroized on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct OpaqueToken {
    text: String,
}

impl OpaqueToken {
    /// A fresh random token. The caller immediately stores only [`OpaqueToken::digest`];
    /// the token itself goes to the user over a secure channel and is not kept.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; TOKEN_SECRET_BYTES];
        crate::util::fill_os_random(&mut bytes);
        Self {
            text: URL_SAFE_NO_PAD.encode(bytes),
        }
    }

    /// Wraps a presented token after checking its shape, so digest comparisons cannot be
    /// polluted by input the generator could never have produced. Wrong shapes are reported
    /// without saying what was wrong.
    pub fn parse(text: &str) -> Result<Self, AuthError> {
        let well_formed = text.len() == OPAQUE_TOKEN_LENGTH
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !well_formed {
            return Err(AuthError::TokenMalformed);
        }
        Ok(Self {
            text: text.to_owned(),
        })
    }

    /// The token text, for digesting it and for the authorization-header round trip.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The SHA-256 digest of the token text: what the caller stores.
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        TokenDigest {
            bytes: sha256(self.text.as_bytes()),
        }
    }
}

impl fmt::Debug for OpaqueToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// A SHA-256 digest of an [`OpaqueToken`] (or of an API-key secret): the at-rest form.
///
/// It is secret material because it verifies a secret in a constant-time comparison, so it
/// is redacted in `Debug` and zeroized on drop like every other secret type in this crate.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct TokenDigest {
    bytes: [u8; 32],
}

impl TokenDigest {
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

    /// Compares against another digest in constant time; the byte length is fixed by the
    /// type, so no length variation is left to leak.
    #[must_use]
    pub fn matches(&self, presented: &TokenDigest) -> bool {
        bool::from(self.bytes.as_slice().ct_eq(&presented.bytes))
    }

    /// Digests a presented token and compares, in one call.
    #[must_use]
    pub fn matches_token(&self, presented: &OpaqueToken) -> bool {
        self.matches(&presented.digest())
    }

    /// Digests opaque secret data (an API-key secret); visible only inside this crate.
    pub(crate) fn of(data: &[u8]) -> Self {
        Self {
            bytes: sha256(data),
        }
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::expect_ok;

    #[test]
    fn generated_tokens_round_trip() {
        let token = OpaqueToken::generate();
        assert_eq!(token.as_str().len(), OPAQUE_TOKEN_LENGTH);
        let digest = token.digest();
        let presented = expect_ok(OpaqueToken::parse(token.as_str()), "parse");
        assert!(digest.matches_token(&presented));
        assert!(digest.matches(&presented.digest()));
    }

    #[test]
    fn other_tokens_do_not_match() {
        let first = OpaqueToken::generate();
        let second = OpaqueToken::generate();
        assert!(!first.digest().matches_token(&second));
    }

    #[test]
    fn parse_rejects_wrong_shapes() {
        let token = OpaqueToken::generate();
        let text = token.as_str();
        let cases = vec![
            String::new(),
            text[..OPAQUE_TOKEN_LENGTH - 1].to_owned(),
            format!("{text}!"),
            // 43 characters, but one is outside the Base64url alphabet.
            "A".repeat(OPAQUE_TOKEN_LENGTH).replace('A', "+"),
        ];
        for text in cases {
            match OpaqueToken::parse(&text) {
                Err(AuthError::TokenMalformed) => {}
                other => panic!("expected TokenMalformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn digests_reconstruct_from_bytes() {
        let digest = OpaqueToken::generate().digest();
        let rebuilt = expect_ok(TokenDigest::from_bytes(digest.as_bytes()), "bytes");
        assert!(digest.matches(&rebuilt));
        match TokenDigest::from_bytes(&[0u8; 31]) {
            Err(AuthError::DigestMalformed) => {}
            other => panic!("expected DigestMalformed, got {other:?}"),
        }
    }

    #[test]
    fn debug_is_redacted() {
        let token = OpaqueToken::generate();
        let rendered = format!("{token:?}");
        assert!(rendered.contains("***"));
        assert!(!rendered.contains(token.as_str()));
    }
}
