//! The seams: where the snapshot comes from and what the node's scope covers.

use crate::error::BootstrapError;
use serde_json::Value;
use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_closure::{Redactor, References};
use std::collections::BTreeMap;

pub use spot_sync_closure::RecordKey;

/// A snapshot at one position, as this crate reads it.
///
/// The trait is synchronous and runtime-free: an adapter over `spot_journal_snapshot::SnapshotReader`
/// (which is async) drives that reader and answers these calls. The two calls are the two reads the
/// bootstrap needs: one forward pass to plan, and keyed fetches to serve a chunk.
pub trait SnapshotSource {
    /// The tenant the snapshot belongs to.
    fn tenant(&self) -> &str;

    /// The snapshot's position, generation and trust marker.
    fn info(&self) -> SnapshotInfo;

    /// Visit every image once, in ascending `(entity, id)` order, holding at most one at a time.
    ///
    /// # Errors
    /// The visitor's error, or `SourceFailed`.
    fn scan(
        &self,
        visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
    ) -> Result<(), BootstrapError>;

    /// The images of exactly `keys` (at most one chunk's worth), in any order. A key the snapshot
    /// does not hold is absent from the answer.
    ///
    /// # Errors
    /// `SourceFailed`.
    fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError>;
}

/// What the node's scope covers. A snapshot is whole-tenant; the node's view is not, so this is
/// asked of **every** record before it is planned and again before it is served.
pub trait ScopeView {
    /// Whether the node may receive this record. Any doubt answers `false`.
    fn covers(&self, key: &RecordKey, image: &Value) -> bool;

    /// A stable text identifying the scope (for example its hash). A cursor made under one scope
    /// does not resume under another.
    fn fingerprint(&self) -> String;
}

/// The seams a session reads through.
#[derive(Clone, Copy)]
pub struct Seams<'a> {
    /// The node's scope.
    pub scope: &'a dyn ScopeView,
    /// The catalog's view of links (ADR-064 section 2).
    pub references: &'a dyn References,
    /// `excluded_fields.down`, applied at serve time (ADR-066 section 2).
    pub redactor: &'a dyn Redactor,
}

/// A snapshot held in memory, for tests and for small hosts.
#[derive(Debug, Clone)]
pub struct MemorySource {
    tenant: String,
    info: SnapshotInfo,
    records: BTreeMap<RecordKey, ImageRecord>,
}

impl MemorySource {
    /// A snapshot of `records` (any order; sorted here).
    #[must_use]
    pub fn new(tenant: &str, info: SnapshotInfo, records: Vec<ImageRecord>) -> Self {
        Self {
            tenant: tenant.to_owned(),
            info,
            records: records
                .into_iter()
                .map(|r| (RecordKey::new(&r.entity, &r.id), r))
                .collect(),
        }
    }
}

impl SnapshotSource for MemorySource {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn info(&self) -> SnapshotInfo {
        self.info
    }

    fn scan(
        &self,
        visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
    ) -> Result<(), BootstrapError> {
        for record in self.records.values() {
            visit(record.clone())?;
        }
        Ok(())
    }

    fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
        Ok(keys
            .iter()
            .filter_map(|k| self.records.get(k).cloned())
            .collect())
    }
}
