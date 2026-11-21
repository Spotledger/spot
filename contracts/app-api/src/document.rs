//! The dynamic document handle (ADR-05) and merge patches (ADR-055).
//!
//! A [`Document`] holds values by field name, so a custom field added at runtime is reachable
//! without a rebuild. It remembers the values it was loaded with, so hooks can ask what changed
//! (`has_changed`) and the host can report `changed_fields` on the update event (ADR-056).

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use spot_storage_contract::{Name, RecordId, Value, Version};

use crate::error::{AppError, AppResult, ErrorClass, FieldPath};

/// The lifecycle state of a document of a posted DocType (ADR-42). Other DocTypes have none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocState {
    /// Editable draft.
    Draft,
    /// Posted: never edited again, except fields allowed on posted documents.
    Posted,
    /// Cancelled after posting.
    Cancelled,
}

impl DocState {
    /// The native API's `_state` value.
    pub fn as_str(self) -> &'static str {
        match self {
            DocState::Draft => "draft",
            DocState::Posted => "posted",
            DocState::Cancelled => "cancelled",
        }
    }
}

/// One child-table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildRow {
    /// The row's own record id (ADR-33).
    pub id: RecordId,
    /// Values by field name.
    pub fields: BTreeMap<Name, Value>,
}

/// The values a document was loaded with: fields and child tables.
type Snapshot = (BTreeMap<Name, Value>, BTreeMap<Name, Vec<ChildRow>>);

/// A document: a parent record plus its child tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    doctype: Name,
    id: RecordId,
    version: Option<Version>,
    state: Option<DocState>,
    fields: BTreeMap<Name, Value>,
    tables: BTreeMap<Name, Vec<ChildRow>>,
    original: Option<Box<Snapshot>>,
}

fn wrong_type(field: &str, wanted: &str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "document.wrong_type",
        format!("field `{field}` does not hold {wanted}"),
    )
}

impl Document {
    /// A new, unsaved document with a caller-chosen id. The host fills defaults on create.
    pub fn new(doctype: Name, id: RecordId) -> Self {
        Self {
            doctype,
            id,
            version: None,
            state: None,
            fields: BTreeMap::new(),
            tables: BTreeMap::new(),
            original: None,
        }
    }

    /// A document as loaded from storage. Hosts use this; it records the loaded values so that
    /// changes can be detected.
    pub fn loaded(
        doctype: Name,
        id: RecordId,
        version: Version,
        state: Option<DocState>,
        fields: BTreeMap<Name, Value>,
        tables: BTreeMap<Name, Vec<ChildRow>>,
    ) -> Self {
        let original = Some(Box::new((fields.clone(), tables.clone())));
        Self {
            doctype,
            id,
            version: Some(version),
            state,
            fields,
            tables,
            original,
        }
    }

    /// The DocType.
    pub fn doctype(&self) -> &Name {
        &self.doctype
    }

    /// The record id.
    pub fn id(&self) -> RecordId {
        self.id
    }

    /// The stored version; `None` before the first save.
    pub fn version(&self) -> Option<Version> {
        self.version
    }

    /// The lifecycle state, for posted DocTypes.
    pub fn state(&self) -> Option<DocState> {
        self.state
    }

    /// Whether the document has never been saved.
    pub fn is_new(&self) -> bool {
        self.version.is_none()
    }

    /// A field's value; `Null` when unset.
    pub fn get(&self, field: &str) -> &Value {
        self.fields
            .iter()
            .find(|(k, _)| k.as_str() == field)
            .map_or(&Value::Null, |(_, v)| v)
    }

    /// Sets a field. Type checks against metadata happen when the host validates the document.
    pub fn set(&mut self, field: Name, value: impl Into<Value>) {
        self.fields.insert(field, value.into());
    }

    /// All field values.
    pub fn fields(&self) -> &BTreeMap<Name, Value> {
        &self.fields
    }

    /// A decimal field; `None` when null.
    pub fn decimal(&self, field: &str) -> AppResult<Option<Decimal>> {
        match self.get(field) {
            Value::Null => Ok(None),
            Value::Decimal(d) => Ok(Some(*d)),
            _ => Err(wrong_type(field, "a decimal")),
        }
    }

    /// A text field; `None` when null.
    pub fn text(&self, field: &str) -> AppResult<Option<&str>> {
        match self.get(field) {
            Value::Null => Ok(None),
            Value::Text(t) => Ok(Some(t)),
            _ => Err(wrong_type(field, "text")),
        }
    }

    /// A link field; `None` when null.
    pub fn link(&self, field: &str) -> AppResult<Option<RecordId>> {
        match self.get(field) {
            Value::Null => Ok(None),
            Value::Id(id) => Ok(Some(*id)),
            _ => Err(wrong_type(field, "a link")),
        }
    }

    /// An integer field; `None` when null.
    pub fn int(&self, field: &str) -> AppResult<Option<i64>> {
        match self.get(field) {
            Value::Null => Ok(None),
            Value::Int(i) => Ok(Some(*i)),
            _ => Err(wrong_type(field, "an integer")),
        }
    }

    /// A child table's rows, in order; empty when the table has none.
    pub fn rows(&self, table: &str) -> &[ChildRow] {
        self.tables
            .iter()
            .find(|(k, _)| k.as_str() == table)
            .map_or(&[], |(_, v)| v.as_slice())
    }

    /// A child table for editing; created empty if missing.
    pub fn rows_mut(&mut self, table: Name) -> &mut Vec<ChildRow> {
        self.tables.entry(table).or_default()
    }

    /// All child tables.
    pub fn tables(&self) -> &BTreeMap<Name, Vec<ChildRow>> {
        &self.tables
    }

    /// Whether `field` (a field or a child table) differs from when the document was loaded.
    /// Every field of a new document counts as changed.
    pub fn has_changed(&self, field: &str) -> bool {
        match &self.original {
            None => true,
            Some(orig) => {
                let now_f = self
                    .fields
                    .iter()
                    .find(|(k, _)| k.as_str() == field)
                    .map(|(_, v)| v);
                let then_f = orig
                    .0
                    .iter()
                    .find(|(k, _)| k.as_str() == field)
                    .map(|(_, v)| v);
                let now_t = self
                    .tables
                    .iter()
                    .find(|(k, _)| k.as_str() == field)
                    .map(|(_, v)| v);
                let then_t = orig
                    .1
                    .iter()
                    .find(|(k, _)| k.as_str() == field)
                    .map(|(_, v)| v);
                norm(now_f) != norm(then_f)
                    || now_t.map_or(&[][..], |v| v) != then_t.map_or(&[][..], |v| v)
            }
        }
    }

    /// Fields and child tables that changed since load, sorted. For a new document, every field
    /// and table that is set.
    pub fn changed_fields(&self) -> Vec<Name> {
        let names: BTreeSet<&Name> = self
            .fields
            .keys()
            .chain(self.tables.keys())
            .chain(
                self.original
                    .iter()
                    .flat_map(|o| o.0.keys().chain(o.1.keys())),
            )
            .collect();
        names
            .into_iter()
            .filter(|n| self.has_changed(n.as_str()))
            .cloned()
            .collect()
    }

    /// Hosts only: records a save, so the document now counts as loaded at `version`.
    pub fn mark_saved(&mut self, version: Version, state: Option<DocState>) {
        self.version = Some(version);
        self.state = state;
        self.original = Some(Box::new((self.fields.clone(), self.tables.clone())));
    }

    /// Applies a merge patch with the native API's semantics (ADR-055):
    /// - only the given fields change, and `Null` sets a field to null;
    /// - a given child table replaces the whole table: rows with a known id are updated in place
    ///   (only the fields the row patch gives), rows with an id this document does not have are
    ///   inserted with that id (clients may choose ids, ADR-055), rows without an id get
    ///   `new_row_id()`, missing rows are removed, and the order follows the patch;
    /// - the same row id twice in one table fails with `document.invalid_row`. An id already used
    ///   by another document is rejected by storage on write (`AlreadyExists`).
    pub fn apply_patch(
        &mut self,
        patch: &DocumentPatch,
        mut new_row_id: impl FnMut() -> RecordId,
    ) -> AppResult<()> {
        let mut rebuilt = BTreeMap::new();
        for (table, rows) in &patch.tables {
            let old: BTreeMap<RecordId, &ChildRow> = self
                .rows(table.as_str())
                .iter()
                .map(|r| (r.id, r))
                .collect();
            let mut out = Vec::with_capacity(rows.len());
            let mut seen = BTreeSet::new();
            for (i, rp) in rows.iter().enumerate() {
                let row = match rp.id {
                    Some(id) if !seen.insert(id) => {
                        return Err(AppError::field(
                            FieldPath::row(
                                table.as_str(),
                                u32::try_from(i + 1).unwrap_or(u32::MAX),
                                "id",
                            ),
                            "document.invalid_row",
                            "The same row appears twice.",
                        ));
                    }
                    Some(id) => match old.get(&id) {
                        Some(existing) => {
                            let mut r = (*existing).clone();
                            r.fields
                                .extend(rp.fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                            r
                        }
                        None => ChildRow {
                            id,
                            fields: rp.fields.clone(),
                        },
                    },
                    None => ChildRow {
                        id: new_row_id(),
                        fields: rp.fields.clone(),
                    },
                };
                out.push(row);
            }
            rebuilt.insert(table.clone(), out);
        }
        for (k, v) in &patch.fields {
            self.fields.insert(k.clone(), v.clone());
        }
        self.tables.extend(rebuilt);
        Ok(())
    }
}

fn norm(v: Option<&Value>) -> &Value {
    v.unwrap_or(&Value::Null)
}

/// A merge patch (the native API's `PATCH` body, decoded).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentPatch {
    /// Fields to set; `Null` sets null. Fields not listed keep their values.
    pub fields: BTreeMap<Name, Value>,
    /// Child tables to replace, in the new order.
    pub tables: BTreeMap<Name, Vec<RowPatch>>,
}

/// One row of a replaced child table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowPatch {
    /// An existing row's id; `None` for a new row.
    pub id: Option<RecordId>,
    /// Fields to set on the row. For an existing row, fields not listed keep their values.
    pub fields: BTreeMap<Name, Value>,
}
