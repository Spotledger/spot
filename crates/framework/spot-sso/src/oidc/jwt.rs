//! Parsing a JOSE compact serialization (RFC 7519 §7.1) into header, claims and signing
//! input, without trusting anything yet.
//!
//! Parsing proves only shape: three Base64url segments, a JSON header with an `alg`, and JSON
//! claims. Whether the token is acceptable is decided later, in order, by
//! [`crate::oidc::flow::complete`]. Headers that name a key the token itself carries — `jku`,
//! `x5u` and an embedded `jwk` — are refused here: a token never chooses its own key.

use serde_json::Value;

use crate::error::{self as e, Result, SsoError};
use crate::jose;

/// A parsed token header, with the fields this crate cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// The signing algorithm name.
    pub alg: String,
    /// The key id, when present.
    pub kid: Option<String>,
}

/// A parsed compact token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedToken {
    /// The parsed header.
    pub header: Header,
    /// The claim set.
    pub claims: Value,
    /// The signing input: the header and payload segments as sent, dot-free.
    pub signing_input: Vec<u8>,
    /// The raw payload segment bytes, for the claim-set hash.
    pub payload: Vec<u8>,
    /// The raw signature bytes.
    pub signature: Vec<u8>,
}

/// Parses a compact JWS into its parts.
///
/// # Errors
///
/// [`e::codes::TOKEN_MALFORMED`] for a wrong number of segments or unreadable JSON;
/// [`e::codes::HEADER_UNTRUSTED`] when the header carries `jku`, `x5u` or `jwk`.
pub fn parse(token: &str) -> Result<ParsedToken> {
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() != 3 || segments.iter().any(|s| s.is_empty()) {
        return Err(malformed());
    }
    let header_bytes = jose::decode(segments[0]).map_err(|_| malformed())?;
    let payload_bytes = jose::decode(segments[1]).map_err(|_| malformed())?;
    let signature = jose::decode(segments[2]).map_err(|_| malformed())?;
    let header_json: Value = serde_json::from_slice(&header_bytes).map_err(|_| malformed())?;
    let claims: Value = serde_json::from_slice(&payload_bytes).map_err(|_| malformed())?;
    if !header_json.is_object() || !claims.is_object() {
        return Err(malformed());
    }
    for name in ["jku", "x5u", "jwk"] {
        if header_json.get(name).is_some() {
            return Err(SsoError::new(
                e::codes::HEADER_UNTRUSTED,
                "The sign-in response could not be trusted.",
                "Configure a fixed `jwks_uri`; a token must not carry its own key.",
            )
            .with_detail(name));
        }
    }
    let alg = header_json
        .get("alg")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?
        .to_owned();
    let kid = header_json
        .get("kid")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let signing_input = format!("{}.{}", segments[0], segments[1]).into_bytes();
    Ok(ParsedToken {
        header: Header { alg, kid },
        claims,
        signing_input,
        payload: payload_bytes,
        signature,
    })
}

/// The string value of a claim.
#[must_use]
pub fn claim_str(claims: &Value, name: &str) -> Option<String> {
    claims.get(name)?.as_str().map(str::to_owned)
}

/// The integer value of a NumericDate claim.
#[must_use]
pub fn claim_i64(claims: &Value, name: &str) -> Option<i64> {
    claims.get(name)?.as_i64()
}

/// The boolean value of a claim.
#[must_use]
pub fn claim_bool(claims: &Value, name: &str) -> Option<bool> {
    claims.get(name)?.as_bool()
}

/// A claim that is a string or an array of strings, as a list.
#[must_use]
pub fn claim_str_list(claims: &Value, name: &str) -> Vec<String> {
    match claims.get(name) {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// The `aud` claim as a list, whether it is a string or an array.
#[must_use]
pub fn audiences(claims: &Value) -> Vec<String> {
    claim_str_list(claims, "aud")
}

fn malformed() -> SsoError {
    SsoError::new(
        e::codes::TOKEN_MALFORMED,
        "The sign-in response could not be read.",
        "Check the identity provider returns a standard compact ID token.",
    )
}
