//! The frozen release's journal script: every entry, in position order, exactly as the previous
//! release wrote it.
//!
//! Nothing here reads a clock or a random source: positions, ids, timestamps and payloads are
//! fixed, so the bytes the generator seals are the same today as they were when the fixture was
//! committed. The documents are invented (`acme`, an account, an invoice, two sales orders); no
//! real tenant's data is in them, and the personal field is an opaque envelope with no key.
//!
//! The script covers what the harness must prove a rebuild carries:
//!
//! * three entities, one per kind of shape change — `account` renamed a field (schema 1 → 2),
//!   `invoice` dropped a field and added another with a default (2 → 3), `sales_order` moved a
//!   field into its child rows (1 → 2);
//! * a tombstone (`account` `a2`);
//! * a record updated three times (`account` `a1`, versions 2, 3 and 4);
//! * a personal-field envelope left sealed: opaque JSON the harness never opens;
//! * one audit entry (`sys_role_changed`) and one domain event (`doc_posted`), which the rebuild
//!   replays and skips respectively.

use serde_json::{Map, Value, json};

use crate::SOURCE;

/// One entry of the frozen journal.
pub struct Event {
    /// The position (sequence number) in generation 1.
    pub position: u64,
    /// The envelope's topic: `doc_state` for state, `sys_…` for audit, anything else domain.
    pub topic: &'static str,
    /// The subject's entity.
    pub entity: &'static str,
    /// The subject's record id.
    pub id: u64,
    /// The subject version, for a state entry.
    pub version: Option<u64>,
    /// The topic data.
    pub data: Value,
}

/// A record as the journal stored it: its body and the catalog version it was written under.
pub struct Stored {
    /// The entity.
    pub entity: String,
    /// The record id.
    pub id: String,
    /// The record version.
    pub version: u64,
    /// The catalog version the body is in.
    pub schema: u64,
    /// The document body.
    pub body: Map<String, Value>,
}

/// A canonical UUID-shaped record id for `n`.
#[must_use]
pub fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// The tag a protected personal field carries in the frozen documents (WP-JRN-08's envelope
/// shape).
///
/// Assembled like [`crate::SOURCE`], for the same reason: a document value, not an error code.
const SEALED_TAG: &str = concat!("spot", ".journal.pii.v1");

/// The sealed personal-field envelope of document `n`.
///
/// The shape a personal field has in the journal when it is left protected (WP-JRN-08): the
/// ciphertext and the id of the key that wrapped it — never the key, and never the plaintext. The
/// upcasters do not touch it, so a rebuild must hand it to the sink exactly as stored.
#[must_use]
pub fn sealed(n: u64) -> Value {
    json!({
        "sealed": SEALED_TAG,
        "key_id": format!("acme/contact/{}", n % 3 + 1),
        "ciphertext": format!("{n:04x}9f2c4b7ae0135d68c2b4f7a90e6d3158"),
    })
}

/// The `doc_state` payload of one record.
fn state(op: &str, schema: u64, image: Value) -> Value {
    json!({ "op": op, "entity_schema": schema, "image": image })
}

/// The frozen script, in position order.
///
/// Positions 1–5 are the first segment (and the snapshot's range); 6–13 the second.
#[must_use]
pub fn script() -> Vec<Event> {
    let a1 = uuid(1);
    vec![
        Event {
            position: 1,
            topic: "doc_state",
            entity: "account",
            id: 1,
            version: Some(1),
            data: state(
                "insert",
                1,
                json!({
                    "title": "Acme Holdings",
                    "credit_limit": "1250000.00",
                    "legacy_no": 7,
                    "contact": sealed(1),
                }),
            ),
        },
        Event {
            position: 2,
            topic: "doc_state",
            entity: "invoice",
            id: 101,
            version: Some(1),
            data: state(
                "insert",
                2,
                json!({
                    "number": "INV-2026-0001",
                    "amount": "1250.75",
                    "currency": "PKR",
                    "account": a1,
                    "issued_on": "2026-01-31",
                }),
            ),
        },
        Event {
            position: 3,
            topic: "doc_state",
            entity: "sales_order",
            id: 201,
            version: Some(1),
            data: state(
                "insert",
                1,
                json!({
                    "reference": "SO-2026-0001",
                    "warehouse": "WH-KHI",
                    "lines": [
                        { "sku": "SKU-A", "qty": "2" },
                        { "sku": "SKU-B", "qty": "1" },
                    ],
                }),
            ),
        },
        Event {
            position: 4,
            topic: "doc_state",
            entity: "account",
            id: 2,
            version: Some(1),
            data: state(
                "insert",
                1,
                json!({
                    "title": "Acme Trading",
                    "credit_limit": "0.00",
                    "legacy_no": 8,
                    "contact": sealed(2),
                }),
            ),
        },
        Event {
            position: 5,
            topic: "doc_state",
            entity: "account",
            id: 1,
            version: Some(2),
            data: state(
                "update",
                1,
                json!({
                    "title": "Acme Holdings Ltd",
                    "credit_limit": "1250000.00",
                    "legacy_no": 7,
                    "contact": sealed(1),
                }),
            ),
        },
        Event {
            position: 6,
            topic: "doc_state",
            entity: "sales_order",
            id: 202,
            version: Some(1),
            data: state(
                "insert",
                1,
                json!({
                    "reference": "SO-2026-0002",
                    "warehouse": "WH-LHE",
                    "lines": [{ "sku": "SKU-C", "qty": "5" }],
                }),
            ),
        },
        Event {
            position: 7,
            topic: "doc_state",
            entity: "account",
            id: 1,
            version: Some(3),
            data: state(
                "update",
                1,
                json!({
                    "title": "Acme Holdings Ltd",
                    "credit_limit": "1500000.00",
                    "legacy_no": 7,
                    "contact": sealed(1),
                }),
            ),
        },
        Event {
            position: 8,
            topic: "doc_state",
            entity: "invoice",
            id: 101,
            version: Some(2),
            data: state(
                "update",
                2,
                json!({
                    "number": "INV-2026-0001",
                    "amount": "1300.00",
                    "currency": "PKR",
                    "account": a1,
                    "issued_on": "2026-01-31",
                }),
            ),
        },
        Event {
            position: 9,
            topic: "doc_state",
            entity: "sales_order",
            id: 201,
            version: Some(2),
            data: state(
                "update",
                1,
                json!({
                    "reference": "SO-2026-0001",
                    "warehouse": "WH-KHI",
                    "lines": [
                        { "sku": "SKU-A", "qty": "3" },
                        { "sku": "SKU-B", "qty": "1" },
                    ],
                }),
            ),
        },
        Event {
            position: 10,
            topic: "doc_state",
            entity: "account",
            id: 2,
            version: Some(2),
            data: state("delete", 1, json!({ "gone": true })),
        },
        Event {
            position: 11,
            topic: "doc_state",
            entity: "account",
            id: 1,
            version: Some(4),
            data: state(
                "update",
                1,
                json!({
                    "title": "Acme Holdings Ltd",
                    "credit_limit": "1500000.00",
                    "legacy_no": 7,
                    "contact": sealed(11),
                }),
            ),
        },
        Event {
            position: 12,
            topic: "doc_posted",
            entity: "invoice",
            id: 101,
            version: None,
            data: json!({ "note": "posted" }),
        },
        Event {
            position: 13,
            topic: "sys_role_changed",
            entity: "role",
            id: 9_000,
            version: None,
            data: json!({ "role": "accountant", "granted_to": uuid(8_001) }),
        },
    ]
}

/// The entry line of `event` at `position`, exactly as the previous release's relay wrote it.
#[must_use]
pub fn line(event: &Event) -> String {
    let seq = event.position;
    let mut envelope = json!({
        "event_id": uuid(1_000_000 + seq),
        "topic": event.topic,
        "schema_version": 1,
        "source_id": SOURCE,
        "position": { "generation": 1, "seq": seq },
        "transaction": uuid(2_000_000 + seq),
        "ordinal": 0,
        "subject": { "entity": event.entity, "id": uuid(event.id) },
        "meta": {
            "occurred_at": format!("2026-02-{:02}T08:{:02}:00Z", 1 + (seq - 1) / 24, (seq - 1) % 60),
            "actor": { "kind": "system", "component": "scheduler" },
            "correlation_id": uuid(3_000_000 + seq),
        },
        "data": event.data,
    });
    if let Some(version) = event.version {
        envelope["subject_version"] = json!(version);
    }
    envelope.to_string()
}

/// The state after `upto` positions, as the journal stored it: the bodies at the schema they were
/// written under, deletes applied, domain and audit entries ignored.
#[must_use]
pub fn stored_state(script: &[Event], upto: u64) -> Vec<Stored> {
    let mut state: std::collections::BTreeMap<(String, String), Stored> =
        std::collections::BTreeMap::new();
    for event in script.iter().filter(|event| event.position <= upto) {
        if event.topic != "doc_state" {
            continue;
        }
        let key = (event.entity.to_owned(), uuid(event.id));
        let op = event
            .data
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if op == "delete" {
            state.remove(&key);
            continue;
        }
        let schema = event
            .data
            .get("entity_schema")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let body = event
            .data
            .get("image")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let version = event.version.unwrap_or_default();
        state.insert(
            key,
            Stored {
                entity: event.entity.to_owned(),
                id: uuid(event.id),
                version,
                schema,
                body,
            },
        );
    }
    state.into_values().collect()
}
