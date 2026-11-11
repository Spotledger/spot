//! [`StorageRecordStore`]: the customisation records in the three platform DocTypes
//! (`custom_doctype`, `custom_field`, `property_setter`) over any storage backend.
//!
//! A [`commit`](RecordStore::commit) is **one storage transaction**: every put and removal of a
//! change lands together or not at all, which is what makes the `pending` intent marker honest.
//! Each record has a stable id derived from its key (a UUIDv7-shaped hash), so a put is an
//! upsert and a rerun after a crash writes the same rows again.
//!
//! The host prepares the backend with [`StorageRecordStore::catalog`] merged into the site's
//! catalogue (the three DocTypes are part of the platform app, so the engine's own catalogue
//! already holds them once the app loads them).

use std::collections::BTreeMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use spot_meta::{Definition, Registry};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, Expr, Isolation, Name, Query, QueryExecutor,
    QueryPath, RecordId, RowLock, Selection, StorageBackend, StorageResult, Transaction, TxOptions,
    Value,
};

use crate::records::{
    DocTypeRecord, FieldRecord, PropertyRecord, RecordKey, RecordOp, RecordState, RecordStore,
    Records, StoreFuture,
};

/// The `Custom DocType` definition, as the platform app ships it.
pub const CUSTOM_DOCTYPE_TOML: &str =
    include_str!("../../../../apps/platform/doctypes/custom_doctype.toml");
/// The `Custom Field` definition.
pub const CUSTOM_FIELD_TOML: &str =
    include_str!("../../../../apps/platform/doctypes/custom_field.toml");
/// The `Property Setter` definition.
pub const PROPERTY_SETTER_TOML: &str =
    include_str!("../../../../apps/platform/doctypes/property_setter.toml");

const DOCTYPE: &str = "custom_doctype";
const FIELD: &str = "custom_field";
const PROPERTY: &str = "property_setter";

/// The three record DocTypes, parsed.
///
/// # Errors
///
/// A message when a shipped definition does not parse (a bug in the platform app).
pub fn definitions() -> Result<Vec<Definition>, String> {
    [
        ("custom_doctype.toml", CUSTOM_DOCTYPE_TOML),
        ("custom_field.toml", CUSTOM_FIELD_TOML),
        ("property_setter.toml", PROPERTY_SETTER_TOML),
    ]
    .into_iter()
    .map(|(file, text)| Definition::parse(file, text).map_err(|e| e.to_string()))
    .collect()
}

/// The records of a site in the three DocTypes of the platform app.
pub struct StorageRecordStore<B: StorageBackend> {
    backend: Arc<B>,
    catalog: Arc<Catalog>,
}

impl<B: StorageBackend> StorageRecordStore<B> {
    /// A store over `backend`, whose `catalog` holds the three record DocTypes.
    #[must_use]
    pub fn new(backend: Arc<B>, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }

    /// A catalogue holding only the three record DocTypes, for a host that has not loaded the
    /// platform app yet (and for tests).
    ///
    /// # Errors
    ///
    /// A message when the shipped definitions do not build.
    pub fn catalog(collation: Collation) -> Result<Catalog, String> {
        let meta = Registry::new(definitions()?)
            .map_err(|errors| {
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            })?
            .effective();
        meta.catalog(collation).map_err(|e| e.to_string())
    }

    async fn read_all(&self, entity: &str) -> StorageResult<Vec<BTreeMap<String, Value>>> {
        let name = Name::new(entity)?;
        let alias = Name::new("r")?;
        let definition = self.catalog.entity(&name)?;
        let columns: Vec<Name> = definition.fields.iter().map(|f| f.name.clone()).collect();
        let mut query = Query::from(&name, &alias, QueryPath::Report);
        for column in &columns {
            query.select.push(Selection {
                expr: Expr::field(&alias, column),
                alias: column.clone(),
            });
        }
        let options =
            TxOptions::new(Name::new("customise_load")?, Isolation::ReadCommitted).read_only();
        let mut tx = self
            .backend
            .begin(Arc::clone(&self.catalog), options)
            .await?;
        let rows = tx.query(&query).await?;
        tx.commit().await?;
        Ok(rows
            .rows
            .into_iter()
            .map(|row| {
                columns
                    .iter()
                    .map(Name::to_string)
                    .zip(row)
                    .collect::<BTreeMap<_, _>>()
            })
            .collect())
    }

    async fn load_records(&self) -> StorageResult<Records> {
        let mut records = Records::default();
        for row in self.read_all(DOCTYPE).await? {
            records.doctypes.push(DocTypeRecord {
                name: text(&row, "doctype_name"),
                module: text(&row, "module"),
                definition: text(&row, "definition"),
                state: state(&row),
                reason: opt(&row, "reason"),
                intent: int(&row, "intent"),
                actor: text(&row, "actor"),
                prior: opt(&row, "prior"),
            });
        }
        for row in self.read_all(FIELD).await? {
            records.fields.push(FieldRecord {
                doctype: text(&row, "doctype"),
                name: text(&row, "field_name"),
                spec: text(&row, "spec"),
                insert_after: opt(&row, "insert_after"),
                state: state(&row),
                reason: opt(&row, "reason"),
                intent: int(&row, "intent"),
                actor: text(&row, "actor"),
            });
        }
        for row in self.read_all(PROPERTY).await? {
            records.properties.push(PropertyRecord {
                doctype: text(&row, "doctype"),
                field: opt(&row, "field"),
                property: text(&row, "property"),
                value: text(&row, "value"),
                state: state(&row),
                reason: opt(&row, "reason"),
                intent: int(&row, "intent"),
                actor: text(&row, "actor"),
                prior: opt(&row, "prior"),
            });
        }
        // The order records were written in: custom fields placed after the same field apply
        // in the order they were added.
        records
            .doctypes
            .sort_by(|a, b| (a.intent, &a.name).cmp(&(b.intent, &b.name)));
        records
            .fields
            .sort_by(|a, b| (a.intent, &a.doctype, &a.name).cmp(&(b.intent, &b.doctype, &b.name)));
        records.properties.sort_by(|a, b| {
            (a.intent, &a.doctype, &a.field, &a.property).cmp(&(
                b.intent,
                &b.doctype,
                &b.field,
                &b.property,
            ))
        });
        Ok(records)
    }

    async fn commit_ops(&self, ops: Vec<RecordOp>) -> StorageResult<()> {
        let options = TxOptions::new(Name::new("customise_write")?, Isolation::ReadCommitted);
        let mut tx = self
            .backend
            .begin(Arc::clone(&self.catalog), options)
            .await?;
        for op in &ops {
            let (entity, id, data) = match op {
                RecordOp::PutDocType(r) => {
                    (DOCTYPE, key_id(&["d", &r.name])?, Some(doctype_fields(r)?))
                }
                RecordOp::PutField(r) => (
                    FIELD,
                    key_id(&["f", &r.doctype, &r.name])?,
                    Some(field_fields(r)?),
                ),
                RecordOp::PutProperty(r) => (
                    PROPERTY,
                    key_id(&[
                        "p",
                        &r.doctype,
                        r.field.as_deref().unwrap_or(""),
                        &r.property,
                    ])?,
                    Some(property_fields(r)?),
                ),
                RecordOp::Remove(RecordKey::DocType(name)) => {
                    (DOCTYPE, key_id(&["d", name])?, None)
                }
                RecordOp::Remove(RecordKey::Field(doctype, name)) => {
                    (FIELD, key_id(&["f", doctype, name])?, None)
                }
                RecordOp::Remove(RecordKey::Property(doctype, field, property)) => (
                    PROPERTY,
                    key_id(&["p", doctype, field.as_deref().unwrap_or(""), property])?,
                    None,
                ),
            };
            let entity = Name::new(entity)?;
            let existing = tx.load(&entity, id, RowLock::None).await?;
            match (data, existing) {
                (Some(fields), Some(old)) => {
                    let doc = DocumentData {
                        entity,
                        id,
                        fields,
                        children: BTreeMap::new(),
                    };
                    tx.update(&doc, old.version).await?;
                }
                (Some(fields), None) => {
                    let doc = DocumentData {
                        entity,
                        id,
                        fields,
                        children: BTreeMap::new(),
                    };
                    tx.insert(&doc).await?;
                }
                (None, Some(old)) => tx.delete(&entity, id, old.version).await?,
                (None, None) => {}
            }
        }
        tx.commit().await
    }
}

impl<B: StorageBackend> RecordStore for StorageRecordStore<B> {
    fn load<'a>(&'a self) -> StoreFuture<'a, Records> {
        Box::pin(async move { self.load_records().await.map_err(|e| e.to_string()) })
    }

    fn commit<'a>(&'a self, ops: Vec<RecordOp>) -> StoreFuture<'a, ()> {
        Box::pin(async move { self.commit_ops(ops).await.map_err(|e| e.to_string()) })
    }
}

impl<B: StorageBackend> std::fmt::Debug for StorageRecordStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageRecordStore").finish_non_exhaustive()
    }
}

/// A stable UUIDv7-shaped id for a record key.
fn key_id(parts: &[&str]) -> StorageResult<RecordId> {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::from_bytes(bytes)
}

/// A row's fields from `(name, value)` pairs; the names are constants of the shipped DocTypes.
fn row(pairs: Vec<(&str, Value)>) -> StorageResult<BTreeMap<Name, Value>> {
    pairs
        .into_iter()
        .map(|(key, value)| Ok((Name::new(key)?, value)))
        .collect()
}

fn text_value(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn opt_value(value: Option<&str>) -> Value {
    value.map_or(Value::Null, text_value)
}

fn int_value(value: u64) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn doctype_fields(r: &DocTypeRecord) -> StorageResult<BTreeMap<Name, Value>> {
    row(vec![
        ("doctype_name", text_value(&r.name)),
        ("module", text_value(&r.module)),
        ("definition", text_value(&r.definition)),
        ("state", text_value(r.state.as_str())),
        ("reason", opt_value(r.reason.as_deref())),
        ("intent", int_value(r.intent)),
        ("actor", text_value(&r.actor)),
        ("prior", opt_value(r.prior.as_deref())),
    ])
}

fn field_fields(r: &FieldRecord) -> StorageResult<BTreeMap<Name, Value>> {
    row(vec![
        ("doctype", text_value(&r.doctype)),
        ("field_name", text_value(&r.name)),
        ("spec", text_value(&r.spec)),
        ("insert_after", opt_value(r.insert_after.as_deref())),
        ("state", text_value(r.state.as_str())),
        ("reason", opt_value(r.reason.as_deref())),
        ("intent", int_value(r.intent)),
        ("actor", text_value(&r.actor)),
    ])
}

fn property_fields(r: &PropertyRecord) -> StorageResult<BTreeMap<Name, Value>> {
    row(vec![
        ("doctype", text_value(&r.doctype)),
        ("field", opt_value(r.field.as_deref())),
        ("property", text_value(&r.property)),
        ("value", text_value(&r.value)),
        ("state", text_value(r.state.as_str())),
        ("reason", opt_value(r.reason.as_deref())),
        ("intent", int_value(r.intent)),
        ("actor", text_value(&r.actor)),
        ("prior", opt_value(r.prior.as_deref())),
    ])
}

fn text(row: &BTreeMap<String, Value>, key: &str) -> String {
    opt(row, key).unwrap_or_default()
}

fn opt(row: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

fn int(row: &BTreeMap<String, Value>, key: &str) -> u64 {
    match row.get(key) {
        Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    }
}

fn state(row: &BTreeMap<String, Value>) -> RecordState {
    opt(row, "state")
        .and_then(|s| RecordState::parse(&s))
        .unwrap_or(RecordState::Failed)
}
