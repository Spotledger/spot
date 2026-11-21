//! Domain-separated canonical bytes: the exact bytes a signature covers.
//!
//! The encoding is `DOMAIN || canonical JSON`. The canonical JSON is a JSON object whose keys are
//! serialised in sorted order (a `BTreeMap` serialises that way whatever the serde_json feature
//! set), with no whitespace, integers only (no floats), and strings as serde_json writes them.
//! Nested objects are built as `BTreeMap`s too, so every level is ordered the same way.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// The domain prefix of a publisher certificate's signed bytes.
pub const CERTIFICATE_DOMAIN: &str = "SPOT-PUBLISHER-CERT-V1\n";
/// The domain prefix of a revocation list's signed bytes.
pub const REVOCATION_DOMAIN: &str = "SPOT-REVOCATION-LIST-V1\n";
/// The domain prefix of a fresh revocation list's signed bytes (`next_update` inside the body).
pub const FRESH_REVOCATION_DOMAIN: &str = "SPOT-REVOCATION-LIST-V2\n";
/// The domain prefix of a bundle signature's signed bytes.
pub const BUNDLE_DOMAIN: &str = "SPOT-BUNDLE-SIGNATURE-V1\n";

/// The signed bytes for `domain` over `fields`.
#[must_use]
pub fn encode(domain: &str, fields: &BTreeMap<&str, Value>) -> Vec<u8> {
    let mut out = domain.as_bytes().to_vec();
    // Serialising a map of plain JSON values cannot fail; an empty tail would fail every
    // verification, so it is never reached in practice.
    out.extend_from_slice(&serde_json::to_vec(fields).unwrap_or_default());
    out
}

/// A nested object, built in sorted key order.
#[must_use]
pub fn object(fields: BTreeMap<&str, Value>) -> Value {
    serde_json::to_value(fields).unwrap_or(Value::Null)
}

/// Standard base64 of `bytes`.
#[must_use]
pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// The bytes of standard base64 `text`, if it is valid.
#[must_use]
pub fn unb64(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}
