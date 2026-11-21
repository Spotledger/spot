//! Incoming webhooks: verify the signature first, then parse, then key the start.
//!
//! The HMAC verifier is `spot_webhooks::signing` reused whole (never re-implemented). The body is
//! parsed **only after** the signature verifies. The start key's source-event id is a
//! caller-supplied idempotency header when there is one, otherwise the SHA-256 of the exact body
//! bytes, so a redelivered request starts nothing twice.

use sha2::{Digest, Sha256};
use spot_webhooks::verify_signature;
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::source::WebhookEvent;

/// One incoming webhook request, before verification.
#[derive(Debug, Clone, Copy)]
pub struct IncomingWebhook<'a> {
    /// The path the request arrived on.
    pub path: &'a str,
    /// The `X-Spot-Signature` header value.
    pub signature: Option<&'a str>,
    /// A caller-supplied idempotency header, when the sender sends one.
    pub idempotency_key: Option<&'a str>,
    /// The exact body bytes received.
    pub body: &'a [u8],
}

/// Verifies an incoming webhook and turns it into a [`WebhookEvent`].
///
/// The signature is checked against the exact `body` bytes and `now`; the body is parsed only
/// once it verifies.
///
/// # Errors
///
/// [`Error::SignatureVerificationFailed`] for a missing, malformed or wrong signature (including
/// a timestamp outside tolerance), [`Error::BodyInvalid`] for a body that is not a JSON object.
pub fn accept(
    secret: &[u8],
    call: &IncomingWebhook<'_>,
    now: OffsetDateTime,
    tolerance_secs: Option<i64>,
) -> Result<WebhookEvent> {
    let signature = call
        .signature
        .ok_or_else(|| Error::SignatureVerificationFailed {
            reason: "the request carries no signature header".to_owned(),
        })?;
    let verified =
        verify_signature(secret, signature, call.body, now, tolerance_secs).map_err(|e| {
            Error::SignatureVerificationFailed {
                reason: e.to_string(),
            }
        })?;
    if !verified {
        return Err(Error::SignatureVerificationFailed {
            reason: "the signature does not match the body".to_owned(),
        });
    }

    // Only now, with the signature verified, is the body parsed.
    let payload: serde_json::Value =
        serde_json::from_slice(call.body).map_err(|e| Error::BodyInvalid {
            reason: e.to_string(),
        })?;
    if !payload.is_object() {
        return Err(Error::BodyInvalid {
            reason: "the body is not a JSON object".to_owned(),
        });
    }

    let idempotency_id = match call.idempotency_key {
        Some(key) if !key.is_empty() => key.to_owned(),
        _ => body_hash(call.body),
    };
    Ok(WebhookEvent {
        path: call.path.to_owned(),
        idempotency_id,
        payload,
        occurred_at: now,
    })
}

/// The SHA-256 of the exact body bytes, as 64 lower-case hex characters.
#[must_use]
pub fn body_hash(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hex(&hasher.finalize())
}

/// Lowercase hex of a digest.
fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_webhooks::signing::{SigningKey, build_signature_header};

    const SECRET: &[u8] = b"a-webhook-secret-value-of-adequate-length";
    const BODY: &[u8] = br#"{"order":"SO-1"}"#;

    fn at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn signed_header(timestamp: i64) -> String {
        let signature = SigningKey::new(SECRET).sign(timestamp, BODY);
        build_signature_header(timestamp, &signature)
    }

    #[test]
    fn a_valid_request_is_accepted() {
        let header = signed_header(1_700_000_000);
        let event = accept(
            SECRET,
            &IncomingWebhook {
                path: "/hooks/intake",
                signature: Some(&header),
                idempotency_key: Some("idem-1"),
                body: BODY,
            },
            at(),
            None,
        )
        .unwrap();
        assert_eq!(event.path, "/hooks/intake");
        assert_eq!(event.idempotency_id, "idem-1");
        assert_eq!(event.payload, serde_json::json!({"order": "SO-1"}));
    }

    #[test]
    fn without_a_header_the_body_hash_keys_the_start() {
        let header = signed_header(1_700_000_000);
        let event = accept(
            SECRET,
            &IncomingWebhook {
                path: "/hooks/intake",
                signature: Some(&header),
                idempotency_key: None,
                body: BODY,
            },
            at(),
            None,
        )
        .unwrap();
        assert_eq!(event.idempotency_id, body_hash(BODY));
        assert_eq!(event.idempotency_id.len(), 64);
    }

    #[test]
    fn a_bad_signature_is_refused_before_the_body_is_parsed() {
        // The body is not JSON; a correct implementation must fail on the signature, never on
        // the body, because it never parses the body before verifying.
        let err = accept(
            SECRET,
            &IncomingWebhook {
                path: "/hooks/intake",
                signature: Some("t=1700000000,v1=deadbeef"),
                idempotency_key: None,
                body: b"not json at all",
            },
            at(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.code(),
            crate::error::TriggerErrorCode::SignatureVerificationFailed
        );
    }

    #[test]
    fn a_tampered_body_fails_the_signature() {
        let header = signed_header(1_700_000_000);
        let err = accept(
            SECRET,
            &IncomingWebhook {
                path: "/hooks/intake",
                signature: Some(&header),
                idempotency_key: None,
                body: br#"{"order":"SO-2"}"#,
            },
            at(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.code(),
            crate::error::TriggerErrorCode::SignatureVerificationFailed
        );
    }

    #[test]
    fn a_non_object_body_is_refused_after_verifying() {
        let body = b"[1,2,3]";
        let signature = SigningKey::new(SECRET).sign(1_700_000_000, body);
        let header = build_signature_header(1_700_000_000, &signature);
        let err = accept(
            SECRET,
            &IncomingWebhook {
                path: "/hooks/intake",
                signature: Some(&header),
                idempotency_key: None,
                body,
            },
            at(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code(), crate::error::TriggerErrorCode::BodyInvalid);
    }

    #[test]
    fn the_body_hash_is_stable_and_body_sensitive() {
        assert_eq!(body_hash(BODY), body_hash(BODY));
        assert_ne!(body_hash(BODY), body_hash(b"other"));
    }
}
