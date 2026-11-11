#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]
//! Shared fixtures: records, a link catalog, scopes and a counting snapshot source.

use serde_json::{Value, json};
use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_bootstrap::{
    BootstrapError, HubHead, MemorySource, NodeState, RecordKey, ScopeView, Seams, SnapshotSource,
};
use spot_sync_closure::{ClosureError, NoRedaction, Redactor, References};
use std::cell::Cell;

pub const TENANT: &str = "t1";
pub const POSITION: u64 = 1_000;
pub const GENERATION: u64 = 3;

pub fn info(verified: bool) -> SnapshotInfo {
    SnapshotInfo {
        position: POSITION,
        generation: GENERATION,
        verified,
        taken_at_millis: 0,
    }
}

pub fn hub() -> HubHead {
    HubHead {
        generation: GENERATION,
        head_position: POSITION + 50,
    }
}

pub fn node(id: &str) -> NodeState {
    NodeState {
        node_id: id.to_owned(),
        tenant: TENANT.to_owned(),
        generation: 0,
        applied_position: None,
    }
}

/// A record whose `links` are `"Entity/id"` strings.
pub fn rec(entity: &str, id: &str, version: u64, links: &[&str]) -> ImageRecord {
    ImageRecord {
        entity: entity.to_owned(),
        id: id.to_owned(),
        version,
        entity_schema: 1,
        image: json!({ "id": id, "links": links, "secret_note": "n" }),
    }
}

pub fn source(records: Vec<ImageRecord>) -> MemorySource {
    MemorySource::new(TENANT, info(true), records)
}

/// Reads `links` from the image.
pub struct Links;

impl References for Links {
    fn references(&self, _entity: &str, image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(image
            .get("links")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|s| s.split_once('/'))
                    .map(|(e, i)| RecordKey::new(e, i))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Covers a fixed list of entities.
pub struct Entities(pub &'static [&'static str]);

impl ScopeView for Entities {
    fn covers(&self, key: &RecordKey, _image: &Value) -> bool {
        self.0.contains(&key.entity.as_str())
    }

    fn fingerprint(&self) -> String {
        self.0.join(",")
    }
}

/// Drops `secret_note` from every image.
pub struct DropSecret;

impl Redactor for DropSecret {
    fn redact_down(&self, _entity: &str, image: &mut Value) {
        if let Some(o) = image.as_object_mut() {
            o.remove("secret_note");
        }
    }
}

pub const SCOPE: Entities = Entities(&["Customer", "Item", "Invoice"]);

pub fn seams<'a>(scope: &'a dyn ScopeView) -> Seams<'a> {
    Seams {
        scope,
        references: &Links,
        redactor: &NoRedaction,
    }
}

/// Counts what the session asks of the snapshot.
pub struct Counting<'a> {
    pub inner: &'a MemorySource,
    pub scans: Cell<usize>,
    pub scanned: Cell<usize>,
    pub fetches: Cell<usize>,
    pub max_fetch: Cell<usize>,
    pub fetched: Cell<usize>,
}

impl<'a> Counting<'a> {
    pub fn new(inner: &'a MemorySource) -> Self {
        Self {
            inner,
            scans: Cell::new(0),
            scanned: Cell::new(0),
            fetches: Cell::new(0),
            max_fetch: Cell::new(0),
            fetched: Cell::new(0),
        }
    }
}

impl SnapshotSource for Counting<'_> {
    fn tenant(&self) -> &str {
        self.inner.tenant()
    }

    fn info(&self) -> SnapshotInfo {
        self.inner.info()
    }

    fn scan(
        &self,
        visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
    ) -> Result<(), BootstrapError> {
        self.scans.set(self.scans.get() + 1);
        self.inner.scan(&mut |r| {
            self.scanned.set(self.scanned.get() + 1);
            visit(r)
        })
    }

    fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
        self.fetches.set(self.fetches.get() + 1);
        self.max_fetch.set(self.max_fetch.get().max(keys.len()));
        self.fetched.set(self.fetched.get() + keys.len());
        self.inner.fetch(keys)
    }
}
