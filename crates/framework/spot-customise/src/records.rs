//! The customisation records: what a site changed, and the state of each change.
//!
//! Three record kinds mirror the platform DocTypes `Custom DocType`, `Custom Field` and
//! `Property Setter`. Every record carries a [`RecordState`] and the `intent` (the change
//! number) that last wrote it, so the pipeline can persist a change as `pending` *before* the
//! schema is touched and `recover()` can finish or roll back whatever a crash left behind.
//!
//! [`RecordStore`] is the persistence seam: `load` reads everything, `commit` applies a list of
//! [`RecordOp`]s **atomically**. [`MemoryRecordStore`] is the in-process store (tests and a
//! single-process start); [`StorageRecordStore`](crate::storage_records::StorageRecordStore) keeps the same records in the three
//! DocTypes over any storage backend.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Where a record is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    /// Persisted, not yet in the schema or the live snapshot. Counts as *wanted*.
    Pending,
    /// In the schema and in the live snapshot.
    Applied,
    /// The change could not be applied; kept with its reason, counts as nothing.
    Failed,
    /// Marked for removal; still in the live snapshot until the removal is applied.
    Removing,
    /// A custom field that was removed. The metadata no longer has it, but the column stays in
    /// the database (a runtime change never drops data), so the record is kept: the schema
    /// plan must keep seeing the column or it would refuse the next change as a drop.
    Removed,
}

impl RecordState {
    /// The text stored in the `state` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Removing => "removing",
            Self::Removed => "removed",
        }
    }

    /// The inverse of [`as_str`](RecordState::as_str).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "applied" => Some(Self::Applied),
            "failed" => Some(Self::Failed),
            "removing" => Some(Self::Removing),
            "removed" => Some(Self::Removed),
            _ => None,
        }
    }
}

/// A DocType created at runtime (the `Custom DocType` DocType).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocTypeRecord {
    /// The DocType's storage name; the record's key.
    pub name: String,
    /// The module it lives in (`custom` unless a platform administrator chose another).
    pub module: String,
    /// The definition, as the TOML text of a `spot-meta` `Definition`.
    pub definition: String,
    /// The state.
    pub state: RecordState,
    /// Why a `failed` record failed (and the last failure of a retried one).
    pub reason: Option<String>,
    /// The change that last wrote this record.
    pub intent: u64,
    /// Who made the change.
    pub actor: String,
    /// The definition before an update that is still `pending`; restored if the update fails.
    pub prior: Option<String>,
}

/// A field added to a DocType (the `Custom Field` DocType).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldRecord {
    /// The DocType the field is on.
    pub doctype: String,
    /// The field's name; with `doctype` the record's key.
    pub name: String,
    /// The field, as the TOML text of a `FieldSpec`.
    pub spec: String,
    /// Place it right after this field; at the end when `None`.
    pub insert_after: Option<String>,
    /// The state.
    pub state: RecordState,
    /// Why a `failed` record failed.
    pub reason: Option<String>,
    /// The change that last wrote this record.
    pub intent: u64,
    /// Who made the change.
    pub actor: String,
}

/// A property override (the `Property Setter` DocType).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyRecord {
    /// The DocType.
    pub doctype: String,
    /// The field, or `None` for a property of the DocType.
    pub field: Option<String>,
    /// The property key (`label`, `read_only`, `options`, ...). With the two above the record's key.
    pub property: String,
    /// The value, as the text form `Property::parse` reads.
    pub value: String,
    /// The state.
    pub state: RecordState,
    /// Why a `failed` record failed.
    pub reason: Option<String>,
    /// The change that last wrote this record.
    pub intent: u64,
    /// Who made the change.
    pub actor: String,
    /// The value before a replacement that is still `pending`; restored if the change fails.
    pub prior: Option<String>,
}

/// Which record an operation names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordKey {
    /// A custom DocType by name.
    DocType(String),
    /// A custom field by DocType and name.
    Field(String, String),
    /// A property override by DocType, field and property key.
    Property(String, Option<String>, String),
}

/// One write of an atomic [`RecordStore::commit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOp {
    /// Inserts or replaces a custom DocType record.
    PutDocType(DocTypeRecord),
    /// Inserts or replaces a custom field record.
    PutField(FieldRecord),
    /// Inserts or replaces a property record.
    PutProperty(PropertyRecord),
    /// Deletes a record.
    Remove(RecordKey),
}

/// Everything the store holds, in the order records were first written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Records {
    /// Custom DocTypes.
    pub doctypes: Vec<DocTypeRecord>,
    /// Custom fields.
    pub fields: Vec<FieldRecord>,
    /// Property overrides.
    pub properties: Vec<PropertyRecord>,
}

impl Records {
    /// Whether any record is `pending` or `removing`: a change a crash left unfinished.
    #[must_use]
    pub fn has_unfinished(&self) -> bool {
        self.doctypes
            .iter()
            .map(|r| r.state)
            .chain(self.fields.iter().map(|r| r.state))
            .chain(self.properties.iter().map(|r| r.state))
            .any(|s| matches!(s, RecordState::Pending | RecordState::Removing))
    }

    /// The custom DocType named `name`.
    #[must_use]
    pub fn doctype(&self, name: &str) -> Option<&DocTypeRecord> {
        self.doctypes.iter().find(|r| r.name == name)
    }

    /// The custom field `doctype.name`.
    #[must_use]
    pub fn field(&self, doctype: &str, name: &str) -> Option<&FieldRecord> {
        self.fields
            .iter()
            .find(|r| r.doctype == doctype && r.name == name)
    }

    /// The property record for `(doctype, field, property)`.
    #[must_use]
    pub fn property(
        &self,
        doctype: &str,
        field: Option<&str>,
        property: &str,
    ) -> Option<&PropertyRecord> {
        self.properties
            .iter()
            .find(|r| r.doctype == doctype && r.field.as_deref() == field && r.property == property)
    }

    /// Applies `ops` in order to a copy: what the store would hold after a successful commit.
    /// A `Put` replaces in place (keeping the record's position) or appends.
    #[must_use]
    pub fn with_ops(&self, ops: &[RecordOp]) -> Records {
        let mut next = self.clone();
        for op in ops {
            next.apply_op(op);
        }
        next
    }

    /// Applies one operation in place.
    pub fn apply_op(&mut self, op: &RecordOp) {
        match op {
            RecordOp::PutDocType(record) => {
                match self.doctypes.iter_mut().find(|r| r.name == record.name) {
                    Some(slot) => *slot = record.clone(),
                    None => self.doctypes.push(record.clone()),
                }
            }
            RecordOp::PutField(record) => {
                match self
                    .fields
                    .iter_mut()
                    .find(|r| r.doctype == record.doctype && r.name == record.name)
                {
                    Some(slot) => *slot = record.clone(),
                    None => self.fields.push(record.clone()),
                }
            }
            RecordOp::PutProperty(record) => {
                match self.properties.iter_mut().find(|r| {
                    r.doctype == record.doctype
                        && r.field == record.field
                        && r.property == record.property
                }) {
                    Some(slot) => *slot = record.clone(),
                    None => self.properties.push(record.clone()),
                }
            }
            RecordOp::Remove(RecordKey::DocType(name)) => {
                self.doctypes.retain(|r| &r.name != name);
            }
            RecordOp::Remove(RecordKey::Field(doctype, name)) => {
                self.fields
                    .retain(|r| !(&r.doctype == doctype && &r.name == name));
            }
            RecordOp::Remove(RecordKey::Property(doctype, field, property)) => {
                self.properties.retain(|r| {
                    !(&r.doctype == doctype && &r.field == field && &r.property == property)
                });
            }
        }
    }
}

/// The future a [`RecordStore`] call returns.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Persistence of the customisation records.
pub trait RecordStore: Send + Sync {
    /// Reads every record.
    fn load<'a>(&'a self) -> StoreFuture<'a, Records>;

    /// Applies `ops` **atomically**: all of them or none.
    fn commit<'a>(&'a self, ops: Vec<RecordOp>) -> StoreFuture<'a, ()>;
}

/// The in-process [`RecordStore`]: one `Arc` of it is "the same database" for the nodes of a test.
#[derive(Debug, Default)]
pub struct MemoryRecordStore {
    records: Mutex<Records>,
}

impl MemoryRecordStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of what the store holds, without a future.
    #[must_use]
    pub fn snapshot(&self) -> Records {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Records> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl RecordStore for MemoryRecordStore {
    fn load<'a>(&'a self) -> StoreFuture<'a, Records> {
        let records = self.snapshot();
        Box::pin(async move { Ok(records) })
    }

    fn commit<'a>(&'a self, ops: Vec<RecordOp>) -> StoreFuture<'a, ()> {
        {
            let mut guard = self.lock();
            let next = guard.with_ops(&ops);
            *guard = next;
        }
        Box::pin(async { Ok(()) })
    }
}
