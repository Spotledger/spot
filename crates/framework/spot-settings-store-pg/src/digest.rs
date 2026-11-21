//! The digest of a stored value, for the history. Secrets digest their reference name.

use sha2::{Digest as _, Sha256};
use spot_settings::StoredValue;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing a hex pair into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The `kind` and the text a value is persisted as: plain values as canonical JSON, a secret as
/// its reference name.
pub(crate) fn persisted(value: &StoredValue) -> (&'static str, String) {
    match value {
        StoredValue::Value(plain) => ("value", serde_json::to_string(plain).unwrap_or_default()),
        StoredValue::Secret(reference) => ("secret_ref", reference.expose().to_owned()),
    }
}

/// SHA-256 (hex) over `kind:text`.
pub(crate) fn digest_of(kind: &str, text: &str) -> String {
    hex(&Sha256::digest(format!("{kind}:{text}").as_bytes()))
}

/// The digest of `value`.
pub(crate) fn of(value: &StoredValue) -> String {
    let (kind, text) = persisted(value);
    digest_of(kind, &text)
}

/// Rebuilds a value from its persisted form; `None` for an unknown kind or bad JSON.
pub(crate) fn restore(kind: &str, text: &str) -> Option<StoredValue> {
    match kind {
        "value" => serde_json::from_str(text).ok().map(StoredValue::Value),
        "secret_ref" => Some(StoredValue::Secret(spot_settings::SecretValue::new(text))),
        _ => None,
    }
}
