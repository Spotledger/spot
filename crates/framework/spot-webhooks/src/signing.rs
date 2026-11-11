//! HMAC-SHA256 signing of webhook payloads and the public verifying function.
//!
//! The header is `X-Spot-Signature: t=<unix>,v1=<hex hmac-sha256(secret, t + "." + body)>`.
//! The body is the exact bytes sent, so a receiver verifies what it received. Comparison is
//! constant-time; the timestamp carries a tolerance (five minutes by default).

use ring::hmac;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;

use crate::{DEFAULT_SIGNATURE_TOLERANCE_SECS, Error, Result};

/// A signing key for HMAC-SHA256.
#[derive(Clone)]
pub struct SigningKey {
    key: hmac::Key,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigningKey([redacted])")
    }
}

impl SigningKey {
    /// A key over `key_bytes`.
    pub fn new(key_bytes: &[u8]) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, key_bytes),
        }
    }

    /// The signature of `timestamp` and the raw `body` bytes.
    pub fn sign(&self, timestamp: i64, body: &[u8]) -> String {
        let mut message = Vec::with_capacity(16 + body.len());
        message.extend_from_slice(timestamp.to_string().as_bytes());
        message.push(b'.');
        message.extend_from_slice(body);
        hex::encode(hmac::sign(&self.key, &message).as_ref())
    }

    /// Whether `signature` is the signature of `timestamp` and `body` (constant-time).
    pub fn verify(&self, timestamp: i64, body: &[u8], signature: &str) -> Result<bool> {
        let expected = self.sign(timestamp, body);
        let (Ok(expected), Ok(provided)) = (hex::decode(&expected), hex::decode(signature)) else {
            return Err(Error::SignatureVerificationFailed {
                reason: "signature is not hex".to_string(),
            });
        };
        if expected.len() != provided.len() {
            return Ok(false);
        }
        Ok(bool::from(expected.ct_eq(&provided)))
    }
}

/// Builds the header value from a timestamp and signature.
pub fn build_signature_header(timestamp: i64, signature: &str) -> String {
    format!("t={timestamp},v1={signature}")
}

/// Parses a header value into its timestamp and signature.
///
/// # Errors
///
/// [`Error::SignatureVerificationFailed`] when the header is not `t=<unix>,v1=<hex>`.
pub fn parse_signature_header(header: &str) -> Result<(i64, String)> {
    let (t, v1) = header
        .split_once(',')
        .ok_or_else(|| Error::SignatureVerificationFailed {
            reason: "missing the comma between t and v1".to_string(),
        })?;
    let timestamp = t
        .strip_prefix("t=")
        .ok_or_else(|| Error::SignatureVerificationFailed {
            reason: "missing the t= prefix".to_string(),
        })?
        .parse::<i64>()
        .map_err(|_| Error::SignatureVerificationFailed {
            reason: "timestamp is not an integer".to_string(),
        })?;
    let signature = v1
        .strip_prefix("v1=")
        .ok_or_else(|| Error::SignatureVerificationFailed {
            reason: "missing the v1= prefix".to_string(),
        })?;
    Ok((timestamp, signature.to_string()))
}

/// Verifies a `X-Spot-Signature` header against the body and the current time.
///
/// `tolerance_secs` defaults to [`DEFAULT_SIGNATURE_TOLERANCE_SECS`]. Returns `Ok(false)` for a
/// wrong signature; an out-of-tolerance timestamp, an unparsable header or a non-hex signature
/// is an [`Error::SignatureVerificationFailed`].
pub fn verify_signature(
    secret: &[u8],
    header: &str,
    body: &[u8],
    now: OffsetDateTime,
    tolerance_secs: Option<i64>,
) -> Result<bool> {
    let tolerance = tolerance_secs.unwrap_or(DEFAULT_SIGNATURE_TOLERANCE_SECS);
    let (timestamp, signature) = parse_signature_header(header)?;
    let drift = now
        .unix_timestamp()
        .saturating_sub(timestamp)
        .saturating_abs();
    if drift > tolerance {
        return Err(Error::SignatureVerificationFailed {
            reason: format!("timestamp outside tolerance ({drift}s > {tolerance}s)"),
        });
    }
    SigningKey::new(secret).verify(timestamp, body, &signature)
}

/// A verifier that accepts the current key and, during a rotation overlap, the previous key.
#[derive(Debug, Clone)]
pub struct SignatureVerifier {
    current: SigningKey,
    previous: Option<SigningKey>,
    previous_expires_at: Option<OffsetDateTime>,
}

impl SignatureVerifier {
    /// A verifier with one key.
    pub fn new(current_key: &[u8]) -> Self {
        Self {
            current: SigningKey::new(current_key),
            previous: None,
            previous_expires_at: None,
        }
    }

    /// A verifier with a current and a previous key; the previous is accepted while
    /// `now < previous_expires_at`.
    pub fn with_rotation(
        current_key: &[u8],
        previous_key: &[u8],
        previous_expires_at: OffsetDateTime,
    ) -> Self {
        Self {
            current: SigningKey::new(current_key),
            previous: Some(SigningKey::new(previous_key)),
            previous_expires_at: Some(previous_expires_at),
        }
    }

    /// Whether `signature` verifies under the current key, or the previous key while it is
    /// still inside the overlap.
    pub fn verify(
        &self,
        timestamp: i64,
        body: &[u8],
        signature: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        if self.current.verify(timestamp, body, signature)? {
            return Ok(true);
        }
        if let (Some(previous), Some(expires)) = (&self.previous, self.previous_expires_at)
            && now < expires
        {
            return previous.verify(timestamp, body, signature);
        }
        Ok(false)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use time::Duration;

    const SECRET: &[u8] = b"a-webhook-secret-value-of-adequate-length";
    const OTHER: &[u8] = b"a-different-secret-also-of-adequate-length";
    const BODY: &[u8] = br#"{"event_id":"e1","data":{"status":"Done"}}"#;

    fn at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn valid_signature_verifies() {
        let key = SigningKey::new(SECRET);
        let signature = key.sign(1_700_000_000, BODY);
        let header = build_signature_header(1_700_000_000, &signature);
        assert!(
            verify_signature(SECRET, &header, BODY, at(), None).unwrap(),
            "the exact body must verify"
        );
    }

    #[test]
    fn tampered_body_fails() {
        let key = SigningKey::new(SECRET);
        let signature = key.sign(1_700_000_000, BODY);
        let header = build_signature_header(1_700_000_000, &signature);
        assert!(!verify_signature(SECRET, &header, b"{\"tampered\":true}", at(), None).unwrap());
    }

    #[test]
    fn wrong_secret_fails() {
        let key = SigningKey::new(SECRET);
        let signature = key.sign(1_700_000_000, BODY);
        let header = build_signature_header(1_700_000_000, &signature);
        assert!(!verify_signature(OTHER, &header, BODY, at(), None).unwrap());
    }

    #[test]
    fn old_timestamp_is_outside_tolerance() {
        let key = SigningKey::new(SECRET);
        let old = 1_700_000_000 - 600;
        let signature = key.sign(old, BODY);
        let header = build_signature_header(old, &signature);
        assert!(verify_signature(SECRET, &header, BODY, at(), Some(300)).is_err());
    }

    #[test]
    fn rotation_overlap_accepts_both_then_expires() {
        let old_signature = SigningKey::new(OTHER).sign(1_700_000_000, BODY);
        let before = SignatureVerifier::with_rotation(SECRET, OTHER, at() + Duration::seconds(60));
        assert!(
            before
                .verify(1_700_000_000, BODY, &old_signature, at())
                .unwrap()
        );

        let after = SignatureVerifier::with_rotation(SECRET, OTHER, at() - Duration::seconds(1));
        assert!(
            !after
                .verify(1_700_000_000, BODY, &old_signature, at())
                .unwrap()
        );
    }

    #[test]
    fn malformed_headers_are_errors() {
        assert!(parse_signature_header("nonsense").is_err());
        assert!(parse_signature_header("t=abc,v1=00").is_err());
        assert!(parse_signature_header("t=1").is_err());
    }
}
