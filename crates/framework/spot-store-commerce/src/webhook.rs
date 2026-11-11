//! Webhook intake: signature verification and idempotent event handling.
//!
//! Provider webhooks are verified with the platform's own HMAC scheme
//! ([`spot_webhooks::signing`]): the header is `t=<unix>,v1=<hex hmac-sha256>` over the
//! exact body bytes, with a timestamp tolerance. The store never trusts an unverified
//! body.
//!
//! Every accepted event is recorded by its provider event id in an [`EventIdStore`], so a
//! **duplicate webhook is handled once**: a provider that retries a delivery cannot issue a
//! second licence or a second ledger order.

use std::collections::BTreeMap;
use std::sync::Mutex;

use time::OffsetDateTime;

use crate::error::{Result, StoreError, codes};

/// The default tolerance for a webhook timestamp, in seconds (five minutes).
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// The idempotency store for provider event ids.
pub trait EventIdStore: Send + Sync {
    /// Records `event_id`; returns `true` when it was new, `false` when it was already
    /// recorded.
    fn record(&self, event_id: &str, now: i64) -> bool;

    /// Whether `event_id` has been recorded.
    fn seen(&self, event_id: &str) -> bool;

    /// Forgets an event id, so a provider retry of an event that could not be applied is
    /// processed instead of being taken for a duplicate.
    fn forget(&self, event_id: &str);
}

/// An in-memory event-id store.
#[derive(Debug, Default)]
pub struct MemoryEventIdStore {
    seen: Mutex<BTreeMap<String, i64>>,
}

impl MemoryEventIdStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many events have been recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl EventIdStore for MemoryEventIdStore {
    fn record(&self, event_id: &str, now: i64) -> bool {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(event_id.to_owned(), now)
            .is_none()
    }

    fn seen(&self, event_id: &str) -> bool {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(event_id)
    }

    fn forget(&self, event_id: &str) {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(event_id);
    }
}

/// The verifying side of a provider's webhook signature.
#[derive(Clone)]
pub struct WebhookVerifier {
    secret: Vec<u8>,
    tolerance: i64,
}

impl std::fmt::Debug for WebhookVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret never appears; only the tolerance is configuration.
        f.debug_struct("WebhookVerifier")
            .field("secret", &"<redacted>")
            .field("tolerance", &self.tolerance)
            .finish()
    }
}

impl WebhookVerifier {
    /// A verifier over `secret` with the default tolerance.
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self {
            secret: secret.to_vec(),
            tolerance: DEFAULT_TOLERANCE_SECS,
        }
    }

    /// A verifier with an explicit tolerance.
    #[must_use]
    pub fn with_tolerance(secret: &[u8], tolerance: i64) -> Self {
        Self {
            secret: secret.to_vec(),
            tolerance,
        }
    }

    /// Verifies a `t=...,v1=...` signature header against the exact `body` bytes.
    ///
    /// # Errors
    /// [`codes::WEBHOOK_SIGNATURE_INVALID`] for a malformed header or a wrong signature,
    /// [`codes::WEBHOOK_STALE`] when the timestamp is outside the tolerance, and
    /// [`codes::WEBHOOK_MALFORMED`] when `now` is not a representable instant.
    pub fn verify(&self, header: &str, body: &[u8], now: i64) -> Result<()> {
        let (timestamp, _) = spot_webhooks::signing::parse_signature_header(header)
            .map_err(|_| invalid_signature())?;
        let drift = now.saturating_sub(timestamp).saturating_abs();
        if drift > self.tolerance {
            return Err(StoreError::new(
                codes::WEBHOOK_STALE,
                "the webhook timestamp is outside the accepted tolerance",
            ));
        }
        let at = OffsetDateTime::from_unix_timestamp(now).map_err(|_| {
            StoreError::new(codes::WEBHOOK_MALFORMED, "the instant is not representable")
        })?;
        match spot_webhooks::verify_signature(&self.secret, header, body, at, Some(self.tolerance))
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(invalid_signature()),
            Err(_) => Err(invalid_signature()),
        }
    }
}

/// Signs a body the way [`WebhookVerifier`] expects: the header for `timestamp` and `body`.
#[must_use]
pub fn sign_payload(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let key = spot_webhooks::SigningKey::new(secret);
    let signature = key.sign(timestamp, body);
    spot_webhooks::signing::build_signature_header(timestamp, &signature)
}

fn invalid_signature() -> StoreError {
    StoreError::new(
        codes::WEBHOOK_SIGNATURE_INVALID,
        "the webhook signature does not verify",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{EventIdStore, MemoryEventIdStore, WebhookVerifier, sign_payload};
    use crate::error::codes;

    const SECRET: &[u8] = b"a-provider-webhook-secret-value";
    const BODY: &[u8] = br#"{"event_id":"evt-1"}"#;

    #[test]
    fn a_signed_body_verifies_and_a_tampered_one_does_not() {
        let verifier = WebhookVerifier::new(SECRET);
        let header = sign_payload(SECRET, 1_000, BODY);
        verifier.verify(&header, BODY, 1_000).unwrap();
        assert_eq!(
            verifier.verify(&header, b"{}", 1_000).unwrap_err().code(),
            codes::WEBHOOK_SIGNATURE_INVALID
        );
        assert_eq!(
            verifier.verify("nonsense", BODY, 1_000).unwrap_err().code(),
            codes::WEBHOOK_SIGNATURE_INVALID
        );
    }

    #[test]
    fn a_stale_timestamp_is_refused() {
        let verifier = WebhookVerifier::with_tolerance(SECRET, 60);
        let header = sign_payload(SECRET, 1_000, BODY);
        assert_eq!(
            verifier.verify(&header, BODY, 2_000).unwrap_err().code(),
            codes::WEBHOOK_STALE
        );
    }

    #[test]
    fn the_event_id_store_records_once() {
        let store = MemoryEventIdStore::new();
        assert!(store.record("evt-1", 100));
        assert!(!store.record("evt-1", 101));
        assert!(store.seen("evt-1"));
        assert!(!store.seen("evt-2"));
        assert_eq!(store.len(), 1);
        store.forget("evt-1");
        assert!(!store.seen("evt-1"));
        assert!(store.record("evt-1", 102));
    }
}
