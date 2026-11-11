//! The small value types the crate is written in: setting keys, plan digests.
//!
//! A [`SettingKey`] is validated on construction, including through serde, so a malformed key
//! cannot enter a desired state from a payload. A [`PlanDigest`] is a domain-separated SHA-256
//! over a plan's canonical text; it is what an approval binds to and what apply recomputes.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_settings::SettingSpec;

use crate::error::Error;

/// The domain prefix of a bulk-plan digest; a plan digest can never collide with another
/// SHA-256 the platform computes.
pub const PLAN_DOMAIN: &[u8] = b"spotledger-desired-state-plan-v1\0";

/// The dotted lower-case key of one setting, for example `runtime.logging.level`.
///
/// The shape is exactly the settings schema's: at least two dot-separated segments, each
/// matching `[a-z][a-z0-9_]*`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SettingKey(String);

impl SettingKey {
    /// The key `text`, when it has the schema's dotted lower-case shape.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidKey`] naming the first offending segment.
    pub fn new(text: impl Into<String>) -> Result<Self, Error> {
        let text = text.into();
        SettingSpec::validate_key(&text).map_err(|reason| Error::InvalidKey {
            key: text.clone(),
            reason,
        })?;
        Ok(Self(text))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for SettingKey {
    type Error = Error;

    fn try_from(text: String) -> Result<Self, Error> {
        Self::new(text)
    }
}

impl TryFrom<&str> for SettingKey {
    type Error = Error;

    fn try_from(text: &str) -> Result<Self, Error> {
        Self::new(text)
    }
}

impl From<SettingKey> for String {
    fn from(key: SettingKey) -> String {
        key.0
    }
}

/// The SHA-256 of a plan's canonical text, lowercase hex, domain separated.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanDigest(String);

impl PlanDigest {
    /// Wraps an already computed lowercase hex digest.
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// Hashes `canonical` under [`PLAN_DOMAIN`].
    #[must_use]
    pub(crate) fn of_canonical(canonical: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PLAN_DOMAIN);
        hasher.update(canonical);
        Self(hex(&hasher.finalize()))
    }

    /// The lowercase hex form.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Lower-case hex, without a hex crate.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_keeps_the_schema_shape() {
        assert!(SettingKey::new("runtime.logging.level").is_ok());
        assert!(SettingKey::new("logging").is_err());
        assert!(SettingKey::new("Runtime.logging").is_err());
    }

    #[test]
    fn an_invalid_key_cannot_be_deserialized() {
        let parsed = serde_json::from_str::<SettingKey>("\"Not.A.Key\"");
        assert!(parsed.is_err());
    }

    #[test]
    fn a_digest_is_domain_separated() {
        let first = PlanDigest::of_canonical(b"a");
        let second = PlanDigest::of_canonical(b"b");
        assert_ne!(first, second);
        assert_eq!(first.as_hex().len(), 64);
        assert!(first.as_hex().chars().all(|c| c.is_ascii_hexdigit()));
    }
}
