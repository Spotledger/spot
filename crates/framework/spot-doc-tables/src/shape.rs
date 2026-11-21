//! The shape of a document's child tables: what `validate::check` does not look at.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{AppError, AppResult, ChildRow, Document, ErrorClass};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType};

/// A row id is used twice (in a table, across tables, or by the document itself), or a
/// multiselect row repeats a target. Reported at the offending row, as `table[n].id` or
/// `table[n].<link field>`.
pub const INVALID_ROW: &str = "document.invalid_row";
/// A row holds a value for a field that belongs to another table's DocType.
pub const INVALID_VALUE: &str = "document.invalid_value";

struct Problem {
    path: FieldPath,
    code: &'static str,
    message: String,
}

/// A table field with the definition of its child DocType.
struct Table<'a> {
    spec: &'a FieldSpec,
    child: &'a Definition,
}

/// Checks the shape of `doc`'s child tables against the effective metadata of `doctype`, and
/// reports **every** problem at once (`document.validation_failed`, one field error each, rows
/// counted from 1 as in `validate::check`).
///
/// - **Row ids are unique in the whole document**: no id twice in a table, none shared between
///   tables, and none equal to the document's own id (`document.invalid_row` at `table[n].id`, `n`
///   the later of the two rows).
/// - **A row holds only its own table's fields.** A value for a field that is declared by another
///   table's DocType and not by this one's is `document.invalid_value` at `table[n].field`. (A name
///   no table declares is `document.unknown_field`, reported by `validate::check`, not here.)
/// - **A `table_multiselect` does not repeat a target**: two rows with the same value in the
///   child DocType's link field are `document.invalid_row` at the later row's link field. Rows
///   without a value are left to `validate::check`.
///
/// The ids check also covers tables the DocType does not declare (they are still part of the
/// document); the field checks cover declared tables only.
///
/// # Errors
///
/// The validation error above, `meta.unknown_doctype` if `doctype` is not in `meta`, or
/// `document.unknown_child_doctype` (class `Internal`) if a table's child DocType is missing from
/// the metadata.
pub fn validate_table_shape(meta: &EffectiveMeta, doctype: &str, doc: &Document) -> AppResult<()> {
    let def = meta.definition(doctype).ok_or_else(|| {
        AppError::new(
            ErrorClass::UnknownDocType,
            "meta.unknown_doctype",
            format!("The DocType `{doctype}` does not exist."),
        )
    })?;
    let mut tables: BTreeMap<&str, Table<'_>> = BTreeMap::new();
    for spec in def.fields.iter().filter(|f| f.ty.is_table()) {
        let target = spec.options.as_deref().unwrap_or("");
        let child = meta.definition(target).ok_or_else(|| {
            AppError::new(
                ErrorClass::Internal,
                "document.unknown_child_doctype",
                format!(
                    "The table `{doctype}.{}` uses the DocType `{target}`, which the metadata does not define.",
                    spec.name
                ),
            )
        })?;
        tables.insert(spec.name.as_str(), Table { spec, child });
    }

    let mut problems = Vec::new();
    row_ids(doc, &mut problems);
    foreign_fields(doc, &tables, &mut problems);
    repeated_targets(doc, &tables, &mut problems);

    let mut problems = problems.into_iter();
    let Some(first) = problems.next() else {
        return Ok(());
    };
    let mut error = AppError::field(first.path, first.code, first.message);
    let mut count = 1;
    for p in problems {
        error = error.with_field(p.path, p.code, p.message);
        count += 1;
    }
    if count > 1 {
        error.message = format!("{count} fields are invalid.");
    }
    Err(error)
}

fn number(index: usize) -> u32 {
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

/// Where an id was first seen.
enum Seen<'a> {
    Document,
    Row { table: &'a Name, number: u32 },
}

fn row_ids(doc: &Document, problems: &mut Vec<Problem>) {
    let mut seen: HashMap<RecordId, Seen<'_>> = HashMap::new();
    seen.insert(doc.id(), Seen::Document);
    for (table, rows) in doc.tables() {
        for (i, row) in rows.iter().enumerate() {
            let n = number(i);
            match seen.get(&row.id) {
                None => {
                    seen.insert(row.id, Seen::Row { table, number: n });
                }
                Some(first) => {
                    let message = match first {
                        Seen::Document => {
                            "This row has the same id as the document itself.".to_owned()
                        }
                        Seen::Row {
                            table: t,
                            number: m,
                        } if *t == table => {
                            format!("This row has the same id as row {m} of this table.")
                        }
                        Seen::Row {
                            table: t,
                            number: m,
                        } => {
                            format!("This row has the same id as {t}[{m}], a row of another table.")
                        }
                    };
                    problems.push(Problem {
                        path: FieldPath::row(table.as_str(), n, "id"),
                        code: INVALID_ROW,
                        message,
                    });
                }
            }
        }
    }
}

fn foreign_fields(doc: &Document, tables: &BTreeMap<&str, Table<'_>>, problems: &mut Vec<Problem>) {
    for (name, rows) in doc.tables() {
        let Some(table) = tables.get(name.as_str()) else {
            continue;
        };
        let own: BTreeSet<&str> = table.child.fields.iter().map(|f| f.name.as_str()).collect();
        for (i, row) in rows.iter().enumerate() {
            for field in row.fields.keys() {
                if own.contains(field.as_str()) {
                    continue;
                }
                // Another table that declares this field, in table-name order.
                let owner = tables.iter().find(|(other, t)| {
                    *other != &name.as_str()
                        && t.child.fields.iter().any(|f| f.name == field.as_str())
                });
                if let Some((other, t)) = owner {
                    problems.push(Problem {
                        path: FieldPath::row(name.as_str(), number(i), field.as_str()),
                        code: INVALID_VALUE,
                        message: format!(
                            "`{field}` is a field of the rows of `{other}` (DocType `{}`), not of `{name}`.",
                            t.child.name
                        ),
                    });
                }
            }
        }
    }
}

fn repeated_targets(
    doc: &Document,
    tables: &BTreeMap<&str, Table<'_>>,
    problems: &mut Vec<Problem>,
) {
    for (name, table) in tables {
        if table.spec.ty != FieldType::TableMultiselect {
            continue;
        }
        // The validator guarantees exactly one link field in a multiselect's child DocType.
        let Some(link) = table.child.fields.iter().find(|f| f.ty == FieldType::Link) else {
            continue;
        };
        let rows: &[ChildRow] = doc.rows(name);
        let mut first: HashMap<RecordId, u32> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            let value = row
                .fields
                .iter()
                .find(|(k, _)| k.as_str() == link.name)
                .map(|(_, v)| v);
            let Some(Value::Id(target)) = value else {
                continue;
            };
            match first.get(target) {
                None => {
                    first.insert(*target, number(i));
                }
                Some(earlier) => problems.push(Problem {
                    path: FieldPath::row(name, number(i), &link.name),
                    code: INVALID_ROW,
                    message: format!("This target is already selected in row {earlier}."),
                }),
            }
        }
    }
}
