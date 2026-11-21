//! The field-level diff of two document images (WP-DOC-07.T1).
//!
//! [`diff_documents`] compares a before and an after image against the effective metadata and
//! produces one [`FieldChange`] per field that differs, ordered by field declaration order then
//! row order, stable. The journal (ADR-061) already records every state change; this builds the
//! *user-facing* history from before/after images — it is not an audit trail, and hiding a
//! value here is presentation, not a security boundary (ADR-068 §7.4).
//!
//! Rules:
//!
//! - a path addresses a child-row field by **row id** (`items[id=<uuid7>].qty`), never by
//!   position, so reordering rows is not a change of every field; a pure reorder produces no
//!   changes at all;
//! - engine-managed fields ([`ENGINE_FIELDS`], ADR-42) are skipped: they are the engine's story,
//!   and the journal already tells it;
//! - a `secret` field records **only that** it changed: both sides carry the [`REDACTED`]
//!   marker, never a value (a secret never travels in a document, ADR-068 §6);
//! - a row's addition or removal is **one** change whose path is the row
//!   (`items[id=<uuid7>]`) and whose present side is the whole row;
//! - `before` / `after` are `None` when that side is absent or null (a value set or cleared);
//! - numeric equality is scale-insensitive (`1.0` equals `1.00`, ADR-12 semantics), JSON
//!   equality is by parsed value (whitespace and key order are not changes), and `Null` and
//!   empty `Text` are different values (STO-10).

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::document::ChildRow;
use spot_app_api::meta::FieldKind;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_meta::{ENGINE_FIELDS, EffectiveMeta};

use crate::wire;

/// The side of a masked change: a value is there, its content is not told.
pub const REDACTED: &str = "••••••";

/// Something changed somewhere in a document image.
///
/// `before` is `None` when the field was absent or null on the earlier image; `after` is `None`
/// when it became absent or null on the later image. For a masked (secret) change both sides
/// carry the [`REDACTED`] marker. For a row's add or remove, the side that has the row carries
/// the whole row as a JSON value, under the path `items[id=<uuid7>]`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FieldChange {
    /// Where: `customer`, `items[id=<uuid7>]` (a whole row's add or remove) or
    /// `items[id=<uuid7>].qty`.
    pub path: String,
    /// The value before, or `None` when absent or null. Masked for a secret field.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::wire::opt_value"
    )]
    pub before: Option<Value>,
    /// The value after, or `None` when it became absent or null. Masked for a secret field.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::wire::opt_value"
    )]
    pub after: Option<Value>,
}

impl FieldChange {
    /// True when this change is a whole row's addition or removal: its path ends at the row
    /// (`items[id=<uuid7>]`) and the side that is present carries a row snapshot.
    #[must_use]
    pub fn is_row_change(&self) -> bool {
        let row_side = |v: Option<&Value>| v.is_some_and(is_row_json);
        self.path.ends_with(']')
            && self.path.contains("[id=")
            && (row_side(self.before.as_ref()) || row_side(self.after.as_ref()))
    }

    /// A display-safe form: a masked change keeps its marker, every other change reports its
    /// values as the value encoding writes them. (The UI localises; this is for logs.)
    #[must_use]
    pub fn describe(&self) -> String {
        let show = |v: Option<&Value>| match v {
            None => "∅".to_owned(),
            Some(Value::Text(t)) if t == REDACTED => format!("«{REDACTED}»"),
            Some(other) => crate::wire::value_to_json(other).to_string(),
        };
        match (&self.before, &self.after) {
            (None, Some(_)) => format!("{} = {}", self.path, show(self.after.as_ref())),
            (Some(_), None) => format!("{} (was {})", self.path, show(self.before.as_ref())),
            _ => format!(
                "{}: {} -> {}",
                self.path,
                show(self.before.as_ref()),
                show(self.after.as_ref())
            ),
        }
    }
}

/// Whether the value is a whole-row snapshot (a JSON object with `id` and `fields`).
fn is_row_json(v: &Value) -> bool {
    match v {
        Value::Json(t) => serde_json::from_str::<serde_json::Value>(t)
            .ok()
            .is_some_and(|j| j.get("id").is_some() && j.get("fields").is_some()),
        _ => false,
    }
}

/// How the crate sees one record shape (the document itself, or one child DocType): which
/// fields exist, in which declaration order, and which of them are `secret`.
#[derive(Debug, Default)]
struct FieldIndex {
    /// Declared fields by name, in declaration order (the sort rank).
    order: BTreeMap<String, usize>,
    /// `secret`-typed fields.
    secret: BTreeSet<String>,
}

impl FieldIndex {
    /// Merges the app-API view's field list (which carries the kinds) with the definition's
    /// declaration order. A name already seen keeps its first position: the view's order wins,
    /// a custom field that only the definition lists lands at the end.
    fn build(
        view_fields: &[spot_app_api::meta::FieldDef],
        def_fields: &[spot_meta::FieldSpec],
    ) -> FieldIndex {
        let mut names: Vec<String> = Vec::new();
        let mut secret = BTreeSet::new();
        for f in view_fields {
            names.push(f.name.as_str().to_owned());
            if matches!(f.kind, FieldKind::Secret) {
                secret.insert(f.name.as_str().to_owned());
            }
        }
        for f in def_fields {
            names.push(f.name.clone());
        }
        let mut order = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for (i, name) in names.into_iter().enumerate() {
            if seen.insert(name.clone()) {
                order.insert(name, i);
            }
        }
        FieldIndex { order, secret }
    }

    /// The sort rank of a field: its declaration position. An unknown name — the definition
    /// moved on while an old image replays — ranks after every declared one, and the stable
    /// sort keeps equal ranks in discovery order.
    fn rank(&self, field: &str) -> usize {
        self.order.get(field).copied().unwrap_or(usize::MAX)
    }
}

/// True for a field the engine manages: it never appears in a diff. `doc_state` lives on the
/// document, not in its fields map, so it cannot appear there; `amended_from` is a declared
/// field of a posted DocType. [`ENGINE_FIELDS`] names both.
fn is_engine(name: &str) -> bool {
    ENGINE_FIELDS.contains(&name)
}

/// How a document image is shaped: which fields exist, which are secrets, which tables exist.
#[derive(Debug, Default)]
struct Shape {
    /// The document's own fields.
    parent: FieldIndex,
    /// Child shape per table field name.
    child: BTreeMap<String, FieldIndex>,
    /// Table field names in declaration order.
    tables: Vec<String>,
    /// Child table field names by declaration rank.
    table_rank: BTreeMap<String, usize>,
}

impl Shape {
    /// Reads the shape from the effective metadata of `doctype`. A DocType outside the metadata
    /// yields the default shape: the images still diff, in discovery order.
    fn of(meta: &EffectiveMeta, doctype: &str) -> Shape {
        // The views are held in a Vec so the borrowed slices live as long as this function.
        let view = meta.doctype(doctype);
        let parent_fields: &[spot_app_api::meta::FieldDef] = match &view {
            Some(v) => &v.fields,
            None => &[],
        };
        let def_fields: &[spot_meta::FieldSpec] = match meta.definition(doctype) {
            Some(d) => &d.fields,
            None => &[],
        };
        let parent = FieldIndex::build(parent_fields, def_fields);
        let mut child = BTreeMap::new();
        let mut tables = Vec::new();
        let mut table_rank = BTreeMap::new();
        for f in parent_fields {
            let child_doctype: Option<&str> = match &f.kind {
                FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                    Some(doctype.as_str())
                }
                _ => None,
            };
            if let Some(child_name) = child_doctype {
                let name = f.name.as_str().to_owned();
                let child_view = meta.doctype(child_name);
                let child_fields: &[spot_app_api::meta::FieldDef] = match &child_view {
                    Some(v) => &v.fields,
                    None => &[],
                };
                let child_spec: &[spot_meta::FieldSpec] = match meta.definition(child_name) {
                    Some(d) => &d.fields,
                    None => &[],
                };
                child.insert(name.clone(), FieldIndex::build(child_fields, child_spec));
                // The table is a field like any other: its rank is its position in the field
                // declaration order, not its count among tables.
                table_rank.insert(name.clone(), parent.rank(&name));
                tables.push(name);
            }
        }
        Shape {
            parent,
            child,
            tables,
            table_rank,
        }
    }

    /// The rank of a table field: its declaration position, unknown names last.
    fn rank_of_table(&self, table: &str) -> usize {
        self.table_rank.get(table).copied().unwrap_or(usize::MAX)
    }
}

/// A change with its stable sort key: `(field or table rank, row position)`.
struct Candidate {
    rank: (usize, usize),
    /// Equal ranks keep discovery order (a stable sort).
    change: FieldChange,
}

/// True when two values are the same value for history purposes (see the module rules).
fn values_equal(before: &Value, after: &Value) -> bool {
    match (before, after) {
        (Value::Json(a), Value::Json(b)) => json_equal(a, b),
        _ => before == after,
    }
}

/// JSON equality by parsed value, so whitespace and key order are not changes.
fn json_equal(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(a),
        serde_json::from_str::<serde_json::Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        // Unparseable JSON compares as text: better to show a change than to hide one.
        _ => a == b,
    }
}

/// A field's value or `Null` when the field is absent. Shared by the document's field map and a
/// row's, which have the same shape.
fn norm<'v>(map: &'v BTreeMap<Name, Value>, field: &str) -> &'v Value {
    map.iter()
        .find(|(k, _)| k.as_str() == field)
        .map_or(&Value::Null, |(_, v)| v)
}

/// One side of a scalar change: `None` for absent or null, so a value being set or cleared is
/// tellable from a value-to-value change.
fn side(v: &Value) -> Option<Value> {
    match v {
        Value::Null => None,
        other => Some(other.clone()),
    }
}

/// A masked change: the fact, never the value.
fn masked_change(path: String) -> FieldChange {
    FieldChange {
        path,
        before: Some(Value::Text(REDACTED.to_owned())),
        after: Some(Value::Text(REDACTED.to_owned())),
    }
}

/// The language-neutral summary parameters of one change, as the timeline renders it:
/// `activity.changed` `{"field", "path", "before", "after"}` with values in the value encoding
/// (a masked value keeps the marker; an absent side is null).
#[must_use]
pub fn change_summary_params(change: &FieldChange) -> serde_json::Value {
    serde_json::json!({
        "path": change.path,
        "before": change.before.as_ref().map(crate::wire::value_to_json),
        "after": change.after.as_ref().map(crate::wire::value_to_json),
    })
}

/// A whole row's add or remove: one change carrying the entire row as JSON, path at the row.
fn whole_row_change(
    table: &str,
    id: RecordId,
    before: Option<&ChildRow>,
    after: Option<&ChildRow>,
) -> FieldChange {
    let row_json = |r: &ChildRow| Value::Json(wire::row::to_json(r).to_string());
    FieldChange {
        path: format!("{table}[id={id}]"),
        before: before.map(row_json),
        after: after.map(row_json),
    }
}

/// Diffs two document images against the effective metadata of their DocType.
///
/// The images are what [`Document::fields`](spot_app_api::Document::fields) and
/// [`Document::tables`](spot_app_api::Document::tables) expose. Child rows match by row id; a
/// row present on only one image is one [`FieldChange`] carrying the whole row; a pure reorder
/// yields no changes. Fields the engine manages are skipped; a `secret` field's change is
/// masked with [`REDACTED`]. Ordered by field declaration order, then row order; the sort is
/// stable.
#[must_use]
pub fn diff_documents(
    before_fields: &BTreeMap<Name, Value>,
    before_tables: &BTreeMap<Name, Vec<ChildRow>>,
    after_fields: &BTreeMap<Name, Value>,
    after_tables: &BTreeMap<Name, Vec<ChildRow>>,
    meta: &EffectiveMeta,
    doctype: &str,
) -> Vec<FieldChange> {
    let shape = Shape::of(meta, doctype);
    let mut out: Vec<Candidate> = Vec::new();

    // Scalar fields of both images, in name order (the rank sort below applies the declaration
    // order; name order here makes discovery deterministic).
    let mut names: BTreeSet<&str> = before_fields.keys().map(Name::as_str).collect();
    names.extend(after_fields.keys().map(Name::as_str));
    for name in names {
        if is_engine(name) || shape.tables.iter().any(|t| t == name) {
            continue;
        }
        let b = norm(before_fields, name);
        let a = norm(after_fields, name);
        if values_equal(b, a) {
            continue;
        }
        let change = if shape.parent.secret.contains(name) {
            masked_change(name.to_owned())
        } else {
            FieldChange {
                path: name.to_owned(),
                before: side(b),
                after: side(a),
            }
        };
        out.push(Candidate {
            rank: (shape.parent.rank(name), 0),
            change,
        });
    }

    // Every table name either image carries: the declared ones in declaration order, unknown
    // ones after them (name order), so a definition that has moved on still diffs completely.
    let mut table_names: Vec<&str> = shape.tables.iter().map(String::as_str).collect();
    let mut unknown: BTreeSet<&str> = before_tables.keys().map(Name::as_str).collect();
    unknown.extend(after_tables.keys().map(Name::as_str));
    for name in unknown {
        if !table_names.contains(&name) && !is_engine(name) {
            table_names.push(name);
        }
    }

    for table in table_names {
        let no_rows: Vec<ChildRow> = Vec::new();
        let tb = table_rows(before_tables, table).unwrap_or(&no_rows);
        let ta = table_rows(after_tables, table).unwrap_or(&no_rows);
        if rows_equal_ignore_order(tb, ta) {
            continue;
        }
        let table_rank = shape.rank_of_table(table);
        let bt: BTreeMap<RecordId, &ChildRow> = tb.iter().map(|r| (r.id, r)).collect();
        let at: BTreeMap<RecordId, &ChildRow> = ta.iter().map(|r| (r.id, r)).collect();

        // Additions and removals: one change per row, carrying the whole row, in that image's
        // own row order.
        for (pos, row) in tb.iter().enumerate() {
            if !at.contains_key(&row.id) {
                out.push(Candidate {
                    rank: (table_rank, pos),
                    change: whole_row_change(table, row.id, Some(row), None),
                });
            }
        }
        for (pos, row) in ta.iter().enumerate() {
            if !bt.contains_key(&row.id) {
                out.push(Candidate {
                    rank: (table_rank, pos),
                    change: whole_row_change(table, row.id, None, Some(row)),
                });
            }
        }

        // Field edits on rows both images share, in the after image's row order — the order a
        // reader of the new state sees.
        for (pos, row) in ta.iter().enumerate() {
            let Some(old) = bt.get(&row.id) else { continue };
            let changes = match shape.child.get(table) {
                Some(child) => diff_row(table, old, row, child),
                // An undeclared child DocType: still diff, in discovery order.
                None => diff_row(table, old, row, &FieldIndex::default()),
            };
            for change in changes {
                out.push(Candidate {
                    rank: (table_rank, pos),
                    change,
                });
            }
        }
    }

    // Stable: equal ranks keep discovery order (a removal of position 1 precedes an addition at
    // position 1 of the same table; per-image row order survives within each).
    out.sort_by_key(|c| c.rank);
    out.into_iter().map(|c| c.change).collect()
}

/// Rows equal when the same ids carry the same fields, whatever the order — reordering alone
/// is therefore no change.
fn rows_equal_ignore_order(a: &[ChildRow], b: &[ChildRow]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let am: BTreeMap<RecordId, &ChildRow> = a.iter().map(|r| (r.id, r)).collect();
    let bm: BTreeMap<RecordId, &ChildRow> = b.iter().map(|r| (r.id, r)).collect();
    am.iter()
        .all(|(id, ra)| bm.get(id).is_some_and(|rb| ra.fields == rb.fields))
}

/// Reads one table's rows from a tables map.
fn table_rows<'m>(tables: &'m BTreeMap<Name, Vec<ChildRow>>, name: &str) -> Option<&'m [ChildRow]> {
    tables
        .iter()
        .find(|(k, _)| k.as_str() == name)
        .map(|(_, v)| v.as_slice())
}

/// Diffs one row pair against the child DocType's declaration order.
fn diff_row(
    table: &str,
    before: &ChildRow,
    after: &ChildRow,
    child: &FieldIndex,
) -> Vec<FieldChange> {
    let mut found: Vec<(usize, FieldChange)> = Vec::new();
    let mut names: BTreeSet<&str> = before.fields.keys().map(Name::as_str).collect();
    names.extend(after.fields.keys().map(Name::as_str));
    for name in names {
        if is_engine(name) {
            continue;
        }
        let b = norm(&before.fields, name);
        let a = norm(&after.fields, name);
        if values_equal(b, a) {
            continue;
        }
        let rank = child.rank(name);
        let path = format!("{table}[id={}].{name}", after.id);
        let change = if child.secret.contains(name) {
            masked_change(path)
        } else {
            FieldChange {
                path,
                before: side(b),
                after: side(a),
            }
        };
        found.push((rank, change));
    }
    // Within one row: field declaration order; ties keep discovery order (name order, which the
    // BTreeSet scan fixes), so the ordering stays stable and deterministic.
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, c)| c).collect()
}

/// Applies scalar [`FieldChange`]s to `fields` (what the property test uses to reproduce the
/// after image). A path into a row or table does not apply here; the caller checks
/// [`FieldChange::is_row_change`] first.
///
/// Returns the changes that did not apply: a `before` that no longer matches (the document
/// moved on since), or a path that is not a valid field name. A well-formed history has none.
#[must_use]
pub fn apply_scalar_changes(
    fields: &mut BTreeMap<Name, Value>,
    changes: &[FieldChange],
) -> Vec<FieldChange> {
    let mut unapplied = Vec::new();
    for change in changes {
        if change.path.contains('[') {
            unapplied.push(change.clone());
            continue;
        }
        let Ok(name) = Name::new(&change.path) else {
            unapplied.push(change.clone());
            continue;
        };
        match (&change.before, &change.after) {
            (Some(b), Some(a)) => {
                if values_equal(norm(fields, &change.path), b) {
                    fields.insert(name, a.clone());
                } else {
                    unapplied.push(change.clone());
                }
            }
            (None, Some(a)) => {
                fields.insert(name, a.clone());
            }
            (_, None) => {
                // The value became absent: remove the entry, so the map stays the exact image
                // shape (an image never stores `Null` entries; `Null` and absent are the same
                // side in a diff, and here that means "no entry").
                fields.remove(&name);
            }
        }
    }
    unapplied
}
