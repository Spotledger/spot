//! Scripted state events: envelope lines, and the model of the database they produce.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use spot_journal_snapshot::ImageRecord;

pub const TENANT: &str = "acme";
pub const SOURCE: &str = "acme-site";
pub const OTHER_TENANT: &str = "beta";
pub const OTHER_SOURCE: &str = "beta-site";
pub const ENTITIES: [&str; 3] = ["account", "customer", "invoice"];

pub fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// One state change: an insert (version 1) or an update of a record of one of three entities.
#[derive(Clone, Debug)]
pub struct Change {
    pub entity: &'static str,
    pub n: u64,
    pub version: u64,
    pub body: Value,
}

/// The database as the script leaves it: `(entity, id)` to `(version, body, position)`.
#[derive(Clone, Debug, Default)]
pub struct Model(pub BTreeMap<(String, String), (u64, Value, u64)>);

impl Model {
    /// The `i`-th change (i from 0): entity `i % 3`, record `(i / 3) % 5 + 1`, next version.
    pub fn next_change(&self, i: u64) -> Change {
        let entity = ENTITIES[usize::try_from(i % 3).unwrap()];
        let n = (i / 3) % 5 + 1;
        let version = self
            .0
            .get(&(entity.to_owned(), uuid(n)))
            .map_or(1, |(v, _, _)| v + 1);
        Change {
            entity,
            n,
            version,
            body: json!({"name": format!("{entity}-{n}-v{version}")}),
        }
    }

    pub fn apply(&mut self, change: &Change, position: u64) {
        self.0.insert(
            (change.entity.to_owned(), uuid(change.n)),
            (change.version, change.body.clone(), position),
        );
    }

    pub fn records(&self) -> Vec<ImageRecord> {
        self.0
            .iter()
            .map(|((entity, id), (version, body, _))| ImageRecord {
                entity: entity.clone(),
                id: id.clone(),
                version: *version,
                entity_schema: 1,
                image: body.clone(),
            })
            .collect()
    }
}

/// The envelope line of a `doc_state` write at `(generation, seq)`.
pub fn state_line(source: &str, generation: u64, seq: u64, change: &Change) -> Vec<u8> {
    let op = if change.version == 1 {
        "insert"
    } else {
        "update"
    };
    json!({
        "event_id": uuid(10_000_000 * generation + seq),
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": source,
        "position": {"generation": generation, "seq": seq},
        "transaction": uuid(20_000_000 * generation + seq),
        "ordinal": 0,
        "subject": {"entity": change.entity, "id": uuid(change.n)},
        "subject_version": change.version,
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": uuid(30_000_000 * generation + seq),
        },
        "data": {"op": op, "entity_schema": 1, "image": change.body},
    })
    .to_string()
    .into_bytes()
}

/// A domain event line (not a state image: a rebuild counts and skips it).
pub fn domain_line(source: &str, generation: u64, seq: u64) -> Vec<u8> {
    json!({
        "event_id": uuid(10_000_000 * generation + seq),
        "topic": "doc_posted",
        "schema_version": 1,
        "source_id": source,
        "position": {"generation": generation, "seq": seq},
        "transaction": uuid(20_000_000 * generation + seq),
        "ordinal": 0,
        "subject": {"entity": "invoice", "id": uuid(1)},
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": uuid(30_000_000 * generation + seq),
        },
        "data": {"note": "sent"},
    })
    .to_string()
    .into_bytes()
}
