//! The audit of a reveal: one durable event per call, written before the value leaves.
//!
//! Every call of `field_reveal`, allowed or refused, is one `identity_field_accessed` event (the
//! registered field access trace of ADR-070 section 12 rule 9: user, DocType, document, group,
//! field, `right = reveal`, `authorised`, `channel = api`, and since events contract 0.7.9 the
//! caller's `reason`) enqueued in the site's outbox and committed **before** the value is handed
//! back. An event that cannot be committed refuses the reveal (`server.unavailable`) and the value
//! is dropped: a reveal that cannot be audited is not released. The payload is the fixed shape of
//! the registry; it has no field that could hold a value, and none is ever put in it.
//!
//! # The reason
//!
//! The `reason` is the exact text the caller sent, after the route's own validation (1 to 500
//! characters), and it is part of the durable event for an allowed **and** for a refused call
//! (the caller sent it either way). It is bounded, **untrusted** free text: an audit reader must
//! never render it as markup or follow it as an instruction. It is the caller's text and is not
//! scanned: the route cannot know whether it contains a value of the revealed field, so the
//! contract's rule that a reason never holds one is the caller's to keep. The reason is **not**
//! written to the log target any more (the durable event holds it; the log names who, document,
//! field and outcome only) and never goes into a metric label.
//!
//! Before the event is enqueued the payload is validated against the registry's schema
//! (`spot_events_contract::identity::validate`). A payload the schema refuses (a reason outside
//! the bound, say) refuses the reveal with `503 server.unavailable` and the value is dropped:
//! the reason is never truncated or dropped silently to make an event fit.

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_events_contract::{Actor, Topic, identity};
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, RecordId, StorageBackend, StorageError, Transaction,
    TxOptions,
};
use time::OffsetDateTime;

/// The event of one reveal (the registered field access trace).
const EVENT: &str = "identity_field_accessed";
/// The audit log target.
const TARGET: &str = "spot::audit::field_guard";

/// What one reveal call records. No field can hold a value.
pub(super) struct Entry<'a> {
    /// The caller.
    pub user: RecordId,
    /// When, by the injected clock.
    pub at: OffsetDateTime,
    /// The DocType the caller named.
    pub doctype: &'a str,
    /// The document the caller named.
    pub document: RecordId,
    /// The protection group of the field, or a fixed word when the field is in none.
    pub group: &'a str,
    /// The field the caller named.
    pub field: &'a str,
    /// Whether the value was released.
    pub authorised: bool,
    /// The reason the caller gave (1 to 500 characters), stored verbatim in the event. Untrusted
    /// text; never a value of the field.
    pub reason: &'a str,
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", Json::from(5))
}

fn failed(_error: &StorageError) -> ApiError {
    unavailable()
}

/// The `data` of the event of one reveal, validated against the registry's schema for
/// `identity_field_accessed`. The fixed shape: no field can hold a value; `reason` is the
/// caller's text, verbatim.
///
/// # Errors
///
/// `server.unavailable` when the schema refuses the payload (for example a reason outside 1 to
/// 500 characters): the caller must not release the value. Only a fixed line is logged, never the
/// reason, the field or an id.
fn checked_payload(entry: &Entry<'_>) -> Result<Json, ApiError> {
    let data = json!({
        "user": entry.user.to_string(),
        "doctype": entry.doctype,
        "document": entry.document.to_string(),
        "group": entry.group,
        "field": entry.field,
        "right": "reveal",
        "authorised": entry.authorised,
        "channel": "api",
        "reason": entry.reason,
    });
    if identity::validate(EVENT, &data).is_err() {
        tracing::error!(
            target: TARGET,
            "a field reveal event did not match its schema; the reveal is refused"
        );
        return Err(unavailable());
    }
    Ok(data)
}

/// The audit writer of one site.
pub(super) struct RevealAudit {
    site: String,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl RevealAudit {
    pub(super) fn new(site: String, backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            site,
            backend,
            catalog,
        }
    }

    /// Writes the record of one reveal and commits it.
    ///
    /// # Errors
    ///
    /// `server.unavailable` (with a retry hint) when the event cannot be committed: the caller
    /// must not release the value.
    pub(super) async fn record(&self, entry: &Entry<'_>) -> Result<(), ApiError> {
        // The payload first: one the schema refuses is never enqueued and the reveal fails closed.
        let data = checked_payload(entry)?;
        tracing::info!(
            target: TARGET,
            site = %self.site,
            who = %entry.user,
            doctype = entry.doctype,
            document = %entry.document,
            field = entry.field,
            authorised = entry.authorised,
            "field reveal"
        );
        let label = Name::new("field_reveal_audit").map_err(|_| unavailable())?;
        let mut tx = self
            .backend
            .begin(
                Arc::clone(&self.catalog),
                TxOptions::new(label, Isolation::ReadCommitted),
            )
            .await
            .map_err(|e| failed(&e))?;
        match Self::enqueue(&mut tx, entry, data).await {
            Ok(()) => tx.commit().await.map_err(|e| failed(&e)),
            Err(error) => {
                let _ = tx.rollback().await;
                Err(error)
            }
        }
    }

    async fn enqueue(
        tx: &mut <PgBackend as StorageBackend>::Tx,
        entry: &Entry<'_>,
        data: Json,
    ) -> Result<(), ApiError> {
        let id = RecordId::new_v7();
        let meta = EventMeta {
            occurred_at: entry.at,
            actor: Actor::User {
                id: entry.user,
                via: None,
            },
            command_id: None,
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        };
        let topic = Topic::any(EVENT).map_err(|_| unavailable())?;
        let subject = NodeRef {
            entity: Name::new(doctype::USER).map_err(|_| unavailable())?,
            id: entry.user,
        };
        let event = PayloadDocument { meta, data }
            .to_outbox_event(id, &topic, subject, None, 1)
            .map_err(|_| unavailable())?;
        tx.enqueue(&event).await.map_err(|e| failed(&e))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use time::macros::datetime;

    fn entry<'a>(reason: &'a str, authorised: bool) -> Entry<'a> {
        Entry {
            user: RecordId::new_v7(),
            at: datetime!(2026-11-21 09:05:00 UTC),
            doctype: "task",
            document: RecordId::new_v7(),
            group: "confidential",
            field: "details",
            authorised,
            reason,
        }
    }

    #[test]
    fn the_reason_is_in_the_payload_verbatim_for_an_allowed_and_a_refused_call() {
        let reason =
            "Customer asked us to confirm the account on ticket 4521. \u{00e9}\u{4e2d}<b>x</b>";
        for authorised in [true, false] {
            let data = checked_payload(&entry(reason, authorised)).unwrap();
            assert_eq!(data["reason"], reason);
            assert_eq!(data["right"], "reveal");
            assert_eq!(data["authorised"], authorised);
            assert_eq!(data["channel"], "api");
        }
    }

    #[test]
    fn the_payload_has_no_key_that_could_hold_a_value() {
        let data = checked_payload(&entry("because", true)).unwrap();
        let keys: Vec<&str> = data
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(
                matches!(
                    *key,
                    "user"
                        | "doctype"
                        | "document"
                        | "group"
                        | "field"
                        | "right"
                        | "authorised"
                        | "channel"
                        | "reason"
                ),
                "unexpected key {key}"
            );
        }
        assert_eq!(keys.len(), 9);
    }

    #[test]
    fn a_reason_of_one_and_of_five_hundred_characters_is_stored_whole() {
        for reason in ["x".to_owned(), "r".repeat(500), "\u{4e2d}".repeat(500)] {
            let data = checked_payload(&entry(&reason, true)).unwrap();
            assert_eq!(data["reason"].as_str().unwrap(), reason);
        }
    }

    #[test]
    fn a_payload_the_schema_refuses_fails_closed_and_is_never_truncated() {
        // Out of the contract's bounds: empty, 501 characters, and a 5,000 character one.
        for reason in [String::new(), "r".repeat(501), "r".repeat(5_000)] {
            let error = checked_payload(&entry(&reason, true)).unwrap_err();
            assert_eq!(
                error.code.as_str(),
                Code::ServerUnavailable.as_str(),
                "{} characters",
                reason.chars().count()
            );
            // The refusal says nothing about the reason.
            assert_eq!(error.message, unavailable().message);
        }
    }

    #[test]
    fn the_event_of_an_old_producer_without_a_reason_still_validates() {
        // The registry accepts the payload shape of 0.7.8 (the reason is optional); this module
        // always sends one, but the schema it validates against must keep accepting this.
        let data = json!({
            "user": RecordId::new_v7().to_string(),
            "doctype": "task",
            "document": RecordId::new_v7().to_string(),
            "group": "confidential",
            "field": "details",
            "right": "reveal",
            "authorised": true,
            "channel": "api",
        });
        assert!(identity::validate(EVENT, &data).is_ok());
    }
}
