//! JSON Web Key Sets (RFC 7517) and the rotation-aware key cache.
//!
//! Only RSA keys of at least 2048 bits and EC keys on P-256 are accepted; anything else is
//! skipped with a note, never turned into a verification that cannot succeed. A [`KeyCache`]
//! keeps keys that were just rotated out until their TTL passes, so a sign-in already in
//! flight when the provider rotates does not fail; an unknown key id after a refresh is a
//! hard refusal, never a fall back to an unverified key.

use ring::signature::{
    ECDSA_P256_SHA256_FIXED, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents, UnparsedPublicKey,
};
use serde::Deserialize;

use crate::error::{self as e, Result, SsoError};
use crate::jose;
use crate::provider::Jwa;

/// The smallest RSA modulus this crate accepts, in bits.
pub const MIN_RSA_BITS: usize = 2048;

/// A public verification key, in the form verification needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublicKey {
    /// An RSA public key: modulus and public exponent, big-endian.
    Rsa {
        /// The modulus `n`.
        n: Vec<u8>,
        /// The public exponent `e`.
        e: Vec<u8>,
    },
    /// A P-256 public key: the affine coordinates.
    Ec {
        /// The `x` coordinate.
        x: [u8; 32],
        /// The `y` coordinate.
        y: [u8; 32],
    },
}

impl PublicKey {
    /// Whether this key is an RSA key.
    #[must_use]
    pub fn is_rsa(&self) -> bool {
        matches!(self, Self::Rsa { .. })
    }

    /// Verifies `signature` over `message` with `alg`, or returns `false`.
    ///
    /// The algorithm must match the key type: an RS256 header is verified only with an RSA
    /// key, an ES256 header only with an EC key, so no algorithm confusion is possible.
    #[must_use]
    pub fn verify(&self, alg: Jwa, message: &[u8], signature: &[u8]) -> bool {
        match (self, alg) {
            (Self::Rsa { n, e }, Jwa::Rs256) => RsaPublicKeyComponents { n, e }
                .verify(&RSA_PKCS1_2048_8192_SHA256, message, signature)
                .is_ok(),
            (Self::Ec { x, y }, Jwa::Es256) => {
                let mut point = [0u8; 65];
                point[0] = 0x04;
                point[1..33].copy_from_slice(x);
                point[33..].copy_from_slice(y);
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
                    .verify(message, signature)
                    .is_ok()
            }
            // Key type and algorithm disagree.
            _ => false,
        }
    }
}

/// One JSON Web Key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jwk {
    /// The key id, when the provider names one.
    pub kid: Option<String>,
    /// The algorithm the provider says this key is for, when stated.
    pub alg: Option<Jwa>,
    /// The public key.
    pub key: PublicKey,
}

/// A key that was not usable, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedKey {
    /// The key id, when it had one.
    pub kid: Option<String>,
    /// Why it was skipped (a stable code where one applies).
    pub reason: &'static str,
}

/// A parsed JSON Web Key Set.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Jwks {
    /// The usable keys.
    pub keys: Vec<Jwk>,
    /// The keys that were skipped, with a reason each.
    pub skipped: Vec<SkippedKey>,
}

#[derive(Deserialize)]
struct RawJwks {
    keys: Vec<serde_json::Value>,
}

/// Parses a JWKS document, skipping unusable keys with a note.
///
/// # Errors
///
/// [`e::codes::JWKS_INVALID`] when the document is not JSON with a `keys` array.
pub fn parse_jwks(bytes: &[u8]) -> Result<Jwks> {
    let raw: RawJwks = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let mut out = Jwks::default();
    for value in raw.keys {
        match parse_key(&value) {
            Ok(Some(jwk)) => out.keys.push(jwk),
            Ok(None) => {}
            Err((kid, reason)) => out.skipped.push(SkippedKey { kid, reason }),
        }
    }
    if out.keys.is_empty() {
        return Err(SsoError::new(
            e::codes::JWKS_INVALID,
            "The identity provider's keys could not be read.",
            "Check the provider publishes RSA (>= 2048 bit) or P-256 keys.",
        ));
    }
    Ok(out)
}

/// Parses one JWK: `Ok(Some)` usable, `Ok(None)` a non-signing key (ignored silently),
/// `Err((kid, reason))` a key this crate cannot use.
#[allow(clippy::type_complexity)]
fn parse_key(
    value: &serde_json::Value,
) -> std::result::Result<Option<Jwk>, (Option<String>, &'static str)> {
    let kid = value.get("kid").and_then(|k| k.as_str()).map(str::to_owned);
    let alg = value
        .get("alg")
        .and_then(|a| a.as_str())
        .and_then(Jwa::parse);
    if let Some(use_) = value.get("use").and_then(|u| u.as_str())
        && use_ != "sig"
    {
        return Ok(None);
    }
    match value.get("kty").and_then(|k| k.as_str()) {
        Some("RSA") => {
            let n = member(value, "n").ok_or((kid.clone(), "missing modulus"))?;
            let e = member(value, "e").ok_or((kid.clone(), "missing exponent"))?;
            if bit_length(&n) < MIN_RSA_BITS {
                return Err((kid, e::codes::KEY_TOO_SMALL));
            }
            Ok(Some(Jwk {
                kid,
                alg,
                key: PublicKey::Rsa { n, e },
            }))
        }
        Some("EC") => {
            if value.get("crv").and_then(|c| c.as_str()) != Some("P-256") {
                return Err((kid, "unsupported curve"));
            }
            let x = member(value, "x").ok_or((kid.clone(), "missing x"))?;
            let y = member(value, "y").ok_or((kid.clone(), "missing y"))?;
            let (Ok(x), Ok(y)) = (
                <[u8; 32]>::try_from(x.as_slice()),
                <[u8; 32]>::try_from(y.as_slice()),
            ) else {
                return Err((kid, "coordinate is not 32 bytes"));
            };
            Ok(Some(Jwk {
                kid,
                alg,
                key: PublicKey::Ec { x, y },
            }))
        }
        _ => Ok(None),
    }
}

/// A base64url member, decoded.
fn member(value: &serde_json::Value, name: &str) -> Option<Vec<u8>> {
    let text = value.get(name)?.as_str()?;
    jose::decode(text).ok()
}

/// The bit length of a big-endian integer, ignoring leading zero bytes.
fn bit_length(bytes: &[u8]) -> usize {
    let first = bytes.iter().position(|b| *b != 0);
    match first {
        None => 0,
        Some(index) => {
            let significant = &bytes[index..];
            (significant.len() - 1) * 8 + (8 - significant[0].leading_zeros() as usize)
        }
    }
}

/// A rotation-aware cache of a provider's keys.
#[derive(Clone, Debug)]
pub struct KeyCache {
    current: Vec<Jwk>,
    previous: Vec<(Jwk, i64)>,
    ttl_seconds: i64,
}

impl KeyCache {
    /// A cache holding `jwks`, keeping retired keys for `ttl_seconds` after rotation.
    #[must_use]
    pub fn new(jwks: Jwks, ttl_seconds: i64) -> Self {
        Self {
            current: jwks.keys,
            previous: Vec::new(),
            ttl_seconds,
        }
    }

    /// The number of current keys.
    #[must_use]
    pub fn current_len(&self) -> usize {
        self.current.len()
    }

    /// Replaces the current keys with `jwks`, keeping the old ones until `now + ttl`.
    pub fn rotate(&mut self, jwks: Jwks, now: i64) {
        let mut previous = std::mem::take(&mut self.previous);
        previous.extend(
            self.current
                .drain(..)
                .map(|key| (key, now + self.ttl_seconds)),
        );
        self.previous = previous;
        self.current = jwks.keys;
    }

    /// Forgets retired keys whose TTL has passed.
    pub fn prune(&mut self, now: i64) {
        self.previous.retain(|(_, expires)| now < *expires);
    }

    /// Finds the key for `kid`, searching current keys and then unexpired retired ones.
    ///
    /// A token without a `kid` is accepted only when exactly one current key exists, so a
    /// missing key id never silently selects among several.
    #[must_use]
    pub fn lookup(&self, kid: Option<&str>, now: i64) -> Option<&Jwk> {
        match kid {
            None => {
                if self.current.len() == 1 {
                    self.current.first()
                } else {
                    None
                }
            }
            Some(kid) => self
                .current
                .iter()
                .find(|key| key.kid.as_deref() == Some(kid))
                .or_else(|| {
                    self.previous
                        .iter()
                        .find(|(key, expires)| key.kid.as_deref() == Some(kid) && now < *expires)
                        .map(|(key, _)| key)
                }),
        }
    }
}

fn invalid() -> SsoError {
    SsoError::new(
        e::codes::JWKS_INVALID,
        "The identity provider's keys could not be read.",
        "Check the provider publishes a valid JWKS at its jwks_uri.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_length_ignores_leading_zeros() {
        assert_eq!(bit_length(&[]), 0);
        assert_eq!(bit_length(&[0, 0]), 0);
        assert_eq!(bit_length(&[0x01]), 1);
        assert_eq!(bit_length(&[0x00, 0x80]), 8);
        assert_eq!(bit_length(&[0x01, 0x00]), 9);
    }

    #[test]
    fn malformed_document_is_refused() {
        let error = match parse_jwks(b"not json") {
            Err(error) => error,
            Ok(_) => panic!("must refuse"),
        };
        assert_eq!(error.code(), e::codes::JWKS_INVALID);
    }
}
