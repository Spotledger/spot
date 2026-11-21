//! The [`MappingPlan`]: file columns to DocType fields (brief item 2).
//!
//! Built from the source's header row and the target DocType's effective metadata, the plan
//! binds every column to a parent field, a child-table field, or nothing (the group key, or
//! an unmapped column). Two ways to bind:
//!
//! - **Auto-match**: the header text, trimmed, compared case-insensitively, against each
//!   field's name and then its label. Nothing else is guessed: `Total excl. tax` does not
//!   match the field `total`.
//! - **Overrides**: the caller names a header text and the field (or `table.field`) it maps
//!   to; an override wins over auto-match for its column.
//!
//! What the plan did **not** map is reported before any row runs: `preflight` (in
//! [`crate::preflight`]) lists the DocType's unmapped fields and the file's unused columns.
//!
//! One column maps to exactly one target and one target takes at most one column: two
//! columns matching the same field is a plan error, not a silent later-wins.

use std::collections::BTreeMap;

use spot_meta::{EffectiveMeta, FieldType};

use crate::error::ImportError;

/// What a column maps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A parent field by name.
    Field(String),
    /// A child-table row field: `(table field name, child field name)`.
    Child(String, String),
    /// The column that starts a new parent (the group key); it carries no value itself.
    GroupKey,
}

/// One column's binding: header text as it appears in the file plus its target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMapping {
    /// The header text.
    pub header: String,
    /// The column's 0-based index.
    pub index: usize,
    /// Where the column's cells go.
    pub target: Target,
}

impl ColumnMapping {
    /// The column's cells feed a parent field.
    pub fn is_parent(&self) -> bool {
        matches!(self.target, Target::Field(_))
    }

    /// The column's cells feed a child row.
    pub fn is_child(&self) -> bool {
        matches!(self.target, Target::Child(..))
    }

    /// The parent field name, or the child table's field name.
    pub fn field_name(&self) -> &str {
        match &self.target {
            Target::Field(name) => name,
            Target::Child(table, _) => table,
            Target::GroupKey => "",
        }
    }
}

/// A field the DocType declares that no column feeds (preflight, brief item 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmappedField {
    /// The field's name.
    pub name: String,
    /// The field's label, or its name.
    pub label: String,
    /// Whether the field is required (an error when unmapped) or optional (a note).
    pub required: bool,
    /// Whether the field belongs to a child table (named `table.field`).
    pub child: bool,
}

/// What preflight found before any row was read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportPreflight {
    /// DocType fields no column feeds.
    pub unmapped_fields: Vec<UnmappedField>,
    /// Columns no field claims (0-based indices, with the header text).
    pub unmapped_columns: Vec<(usize, String)>,
    /// The column the group key was bound to, when grouped.
    pub group_key_column: Option<usize>,
    /// Problems that stop the import (`meta.*`, plan-shape failures) — none left means the
    /// plan stands.
    pub errors: Vec<ImportError>,
}

impl ImportPreflight {
    /// Required fields with no column: these block a usable import.
    pub fn blocking_fields(&self) -> Vec<&UnmappedField> {
        self.unmapped_fields.iter().filter(|f| f.required).collect()
    }

    /// Whether preflight found anything the user must look at (blocking fields, unmapped
    /// columns or plan errors on a non-empty file).
    pub fn has_findings(&self) -> bool {
        !self.unmapped_fields.is_empty()
            || !self.unmapped_columns.is_empty()
            || !self.errors.is_empty()
    }
}

/// An explicit column binding the caller asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Override {
    /// The header text to bind (compared trimmed and case-insensitively).
    pub header: String,
    /// Where its cells go: `"field"`, `"table.field"` or `"group_key"`.
    pub target: String,
}

impl Override {
    /// Binds the header text to a target string.
    pub fn new(header: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            target: target.into(),
        }
    }
}

/// The plan: every column's target (or none), built once and reused for every row.
///
/// Built over a source's headers and the DocType's effective metadata. The plan is also the
/// owner of the conversion choices for the run (bool words, select case), carried here so a
/// caller builds one object per import.
#[derive(Clone, Debug)]
pub struct MappingPlan {
    /// Per column: where its cells go (index-aligned with the source's headers).
    by_column: Vec<Option<Target>>,
    /// The headers (for error messages and the report).
    headers: Vec<String>,
    /// The target DocType.
    pub doctype: String,
    /// The DocType's table fields by name (child-table definitions live in the metadata).
    tables: Vec<String>,
    /// Conversion options this run uses.
    pub options: crate::convert::ConversionOptions,
    /// Formats for date/datetime/time cells.
    pub formats: crate::convert::Formats,
    /// Field specs by qualified name (`field`, `table.field`), resolved at build time so
    /// converting a cell never re-resolves metadata.
    pub specs: BTreeMap<String, spot_meta::FieldSpec>,
    /// The group key's column, when grouped.
    group_key: Option<usize>,
}

impl MappingPlan {
    /// Builds the plan for `doctype` over `headers`: overrides win, then auto-match; the
    /// group key is bound by override only.
    ///
    /// Formats come from the caller (explicit date formats, brief item 4; no day/month
    /// guessing). Unknown doctype, unknown fields or ambiguous matches fail before any row
    /// is read.
    pub fn build(
        meta: &EffectiveMeta,
        doctype: &str,
        headers: &[String],
        overrides: &[Override],
        options: crate::convert::ConversionOptions,
        formats: crate::convert::Formats,
    ) -> Result<Self, ImportError> {
        let def = meta
            .definition(doctype)
            .ok_or_else(|| ImportError::UnknownDoctype {
                doctype: doctype.to_owned(),
            })?;
        let mut by_column: Vec<Option<Target>> = vec![None; headers.len()];
        let mut group_key = None;

        // 1. Overrides: the caller's word is strongest.
        for o in overrides {
            let target = match o.target.as_str() {
                "group_key" => Target::GroupKey,
                _ => {
                    if let Some((table, field)) = o.target.split_once('.') {
                        // A child-table field: `table.field`.
                        let table_field = def
                            .fields
                            .iter()
                            .find(|f| f.name == table && f.ty.is_table())
                            .ok_or_else(|| ImportError::Meta {
                                message: format!(
                                    "the override `{}` names `{table}`, which is not a table of `{doctype}`",
                                    o.header
                                ),
                            })?;
                        let child = table_field.options.as_deref().unwrap_or("");
                        let child_def =
                            meta.definition(child).ok_or_else(|| ImportError::Meta {
                                message: format!(
                                    "the table `{table}` uses the DocType `{child}`, which the metadata does not define"
                                ),
                            })?;
                        if !child_def
                            .fields
                            .iter()
                            .any(|f| f.name == field && !f.ty.is_table())
                        {
                            return Err(ImportError::Meta {
                                message: format!(
                                    "the override `{}` names `{field}`, which is not a field of the child DocType `{child}`",
                                    o.header
                                ),
                            });
                        }
                        Target::Child(table.to_owned(), field.to_owned())
                    } else if def
                        .fields
                        .iter()
                        .any(|f| f.name == o.target && !f.ty.is_table())
                    {
                        // A parent field by name.
                        Target::Field(o.target.clone())
                    } else {
                        return Err(ImportError::Meta {
                            message: format!(
                                "the override `{}` names `{}`, which is not a field of `{doctype}`",
                                o.header, o.target
                            ),
                        });
                    }
                }
            };
            let Some((index, _)) = headers
                .iter()
                .enumerate()
                .find(|(_, h)| h.trim().eq_ignore_ascii_case(o.header.trim()))
            else {
                return Err(ImportError::Meta {
                    message: format!(
                        "the override names header `{}`, which the file does not have (headers: {})",
                        o.header,
                        headers.join(", ")
                    ),
                });
            };
            if let Some(group) = &group_key
                && Target::GroupKey == target
            {
                return Err(ImportError::Meta {
                    message: format!("two columns are bound as the group key: {group} and {index}"),
                });
            }
            if Target::GroupKey == target {
                group_key = Some(index);
            }
            if by_column[index].is_some() {
                return Err(ImportError::Meta {
                    message: format!(
                        "the override `{}` targets column {index}, which another override already took",
                        o.header
                    ),
                });
            }
            by_column[index] = Some(target);
        }

        // 2. Auto-match: trim + case-insensitive against field names, then labels.
        for (index, header) in headers.iter().enumerate() {
            if by_column[index].is_some() {
                continue;
            }
            let key = header.trim();
            if key.is_empty() {
                continue;
            }
            let matches_name = |name: &str| key.eq_ignore_ascii_case(name.trim());
            let field = def
                .fields
                .iter()
                .filter(|f| {
                    !f.ty.is_table()
                        && f.ty != FieldType::Secret
                        && !(f.read_only && f.name == "doc_state")
                })
                .find(|f| matches_name(&f.name))
                .or_else(|| {
                    def.fields
                        .iter()
                        .filter(|f| !f.ty.is_table() && f.ty != FieldType::Secret)
                        .find(|f| matches_name(f.label_or_name()))
                });
            if let Some(f) = field {
                // One target takes at most one column (this module's contract): an override
                // or an earlier header that already feeds this field is a plan error, not
                // silent later-wins.
                let took = by_column
                    .iter()
                    .flatten()
                    .any(|t| matches!(t, Target::Field(n) if n.as_str() == f.name.as_str()));
                if took {
                    return Err(ImportError::Meta {
                        message: format!(
                            "the header `{header}` matches the field `{}`, which another column already feeds; a field takes at most one column — drop the duplicate or rename it",
                            f.name
                        ),
                    });
                }
                by_column[index] = Some(Target::Field(f.name.clone()));
                continue;
            }
            // Child tables then child fields: `table` or `table.field` as a header text is
            // not a thing — a child column's header matches the CHILD's field names and
            // labels. A column matching a child field name binds to that (table, field);
            // ambiguity (two tables share the field name) is a plan error.
            let mut child_hits: Vec<(usize, &str)> = Vec::new();
            for (ti, f) in def
                .fields
                .iter()
                .enumerate()
                .filter(|(_, f)| f.ty.is_table())
            {
                let child = meta.definition(f.options.as_deref().unwrap_or(""));
                if let Some(child_def) = child
                    && child_def
                        .fields
                        .iter()
                        .filter(|cf| !cf.ty.is_table())
                        .any(|cf| matches_name(&cf.name) || matches_name(cf.label_or_name()))
                {
                    child_hits.push((ti, &f.name));
                }
            }
            match child_hits.len() {
                0 => {}
                1 => {
                    let table = child_hits[0].1;
                    let table_field = def.fields.iter().find(|f| f.name == table);
                    let child = table_field
                        .and_then(|f| meta.definition(f.options.as_deref().unwrap_or("")));
                    let child_field = child.and_then(|cd| {
                        cd.fields
                            .iter()
                            .find(|cf| matches_name(&cf.name))
                            .or_else(|| {
                                cd.fields.iter().find(|cf| matches_name(cf.label_or_name()))
                            })
                            .cloned()
                    });
                    let Some(child_field) = child_field else {
                        // Unreachable: the `child_hits` pass matched this field above.
                        continue;
                    };
                    // Same contract as parent fields: one (table, field) target takes at
                    // most one column.
                    let took = by_column.iter().flatten().any(|t| {
                        matches!(t, Target::Child(n, f) if n.as_str() == table && f.as_str() == child_field.name.as_str())
                    });
                    if took {
                        return Err(ImportError::Meta {
                            message: format!(
                                "the header `{header}` matches `{table}.{}`, which another column already feeds; a field takes at most one column — drop the duplicate or rename it",
                                child_field.name
                            ),
                        });
                    }
                    by_column[index] = Some(Target::Child(table.to_owned(), child_field.name));
                }
                _ => {
                    let tables = child_hits
                        .iter()
                        .map(|(_, t)| *t)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(ImportError::Meta {
                        message: format!(
                            "the header `{header}` matches child fields of more than one table: {tables}; use an override"
                        ),
                    });
                }
            }
        }

        let tables = def
            .fields
            .iter()
            .filter(|f| f.ty.is_table())
            .map(|f| f.name.clone())
            .collect();
        let mut specs: BTreeMap<String, spot_meta::FieldSpec> = BTreeMap::new();
        for f in &def.fields {
            match f.ty {
                FieldType::Table => {
                    let child = f.options.as_deref().unwrap_or("");
                    if let Some(child_def) = meta.definition(child) {
                        for cf in &child_def.fields {
                            if !cf.ty.is_table() {
                                specs.insert(format!("{}.{}", f.name, cf.name), cf.clone());
                            }
                        }
                    }
                }
                _ => {
                    specs.insert(f.name.clone(), f.clone());
                }
            }
        }
        Ok(Self {
            by_column,
            headers: headers.to_owned(),
            doctype: doctype.to_owned(),
            tables,
            options,
            formats,
            specs,
            group_key,
        })
    }

    /// The target DocType.
    pub fn doctype(&self) -> &str {
        &self.doctype
    }

    /// The source's headers.
    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    /// The column bound as the group key, when grouped.
    pub fn group_key(&self) -> Option<usize> {
        self.group_key
    }

    /// The conversion options this run uses.
    pub fn options(&self) -> &crate::convert::ConversionOptions {
        &self.options
    }

    /// The compiled formats this run uses.
    pub fn formats(&self) -> &crate::convert::Formats {
        &self.formats
    }

    /// The target of column `index`, or `None` when unmapped.
    pub fn target(&self, index: usize) -> Option<&Target> {
        self.by_column.get(index).and_then(|t| t.as_ref())
    }

    /// Every bound column, in file order.
    pub fn columns(&self) -> impl Iterator<Item = ColumnMapping> + '_ {
        self.by_column.iter().enumerate().filter_map(|(index, t)| {
            t.as_ref().map(|target| ColumnMapping {
                header: self.headers[index].clone(),
                index,
                target: target.clone(),
            })
        })
    }

    /// Whether any column feeds child rows of `table`.
    pub fn has_child_for(&self, table: &str) -> bool {
        self.by_column
            .iter()
            .flatten()
            .any(|t| matches!(t, Target::Child(name, _) if name == table))
    }

    /// The child tables the plan's columns feed.
    pub fn child_tables(&self) -> &[String] {
        &self.tables
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::convert::{ConversionOptions, Formats};
    use crate::testmeta;

    fn formats() -> Formats {
        let (d, dt, t) = Formats::default_lists();
        Formats::build(&d, &dt, &t).unwrap()
    }

    /// A plan over the invoice doctype.
    fn plan(headers: &[&str]) -> MappingPlan {
        let meta = testmeta::meta();
        let headers: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
        MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap()
    }

    /// Auto-match: names, case-insensitivity, trims, labels.
    #[test]
    fn auto_match_names_labels_case_and_trim() {
        let p = plan(&["NUMBER", " customer ", "Status", "Due Date"]);
        // Exact name, case-insensitive.
        assert_eq!(
            p.target(0),
            Some(&Target::Field("number".to_owned())),
            "header case is folded"
        );
        // A name with surrounding spaces matches trimmed.
        assert_eq!(p.target(1), Some(&Target::Field("customer".to_owned())));
        // Another name.
        assert_eq!(p.target(2), Some(&Target::Field("status".to_owned())));
        // The LABEL `Due Date` matches the field `due_date`.
        assert_eq!(p.target(3), Some(&Target::Field("due_date".to_owned())));
        assert_eq!(p.doctype(), "sales_invoice");
    }

    /// A child column's header matches the CHILD's field name or label, bound as
    /// `table.field`.
    #[test]
    fn auto_match_child_fields_by_child_names() {
        let p = plan(&["number", "item", "qty"]);
        assert_eq!(
            p.target(1),
            Some(&Target::Child("items".to_owned(), "item".to_owned()))
        );
        assert_eq!(
            p.target(2),
            Some(&Target::Child("items".to_owned(), "qty".to_owned()))
        );
        assert!(p.has_child_for("items"));
    }

    /// An override wins over auto-match and can bind anything, including the group key;
    /// unknown override targets and missing headers are plan errors.
    #[test]
    fn overrides_win_and_are_validated() {
        let meta = testmeta::meta();
        let headers = vec!["Total excl. tax".to_owned(), "Inv No".to_owned()];
        let overrides = vec![
            Override::new("Total excl. tax", "total"),
            Override::new("Inv No", "number"),
        ];
        let p = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &overrides,
            ConversionOptions::default(),
            formats(),
        )
        .unwrap();
        // `Total excl. tax` matches nothing by name or label (spaces are not folded) —
        // only the override binds it.
        assert_eq!(p.target(0), Some(&Target::Field("total".to_owned())));
        assert_eq!(p.target(1), Some(&Target::Field("number".to_owned())));

        // An unknown target fails the build.
        let bad = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[Override::new("Inv No", "nonsense")],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap_err();
        assert!(matches!(bad, ImportError::Meta { .. }), "{bad:?}");

        // A header the file does not have fails the build.
        let bad = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[Override::new("no such column", "number")],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap_err();
        assert!(matches!(bad, ImportError::Meta { .. }), "{bad:?}");
    }

    /// Two columns matching the same field is a plan error, not silent later-wins; same
    /// for two columns reaching one child field.
    #[test]
    fn two_columns_one_field_is_a_plan_error() {
        let meta = testmeta::meta();
        let parents: Vec<String> = ["number", "Number"]
            .iter()
            .map(|h| (*h).to_owned())
            .collect();
        let e = MappingPlan::build(
            &meta,
            "sales_invoice",
            &parents,
            &[],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap_err();
        assert!(
            matches!(e, ImportError::Meta { .. }),
            "duplicate parent binding must fail: {e:?}"
        );

        let children: Vec<String> = ["item", "Item"].iter().map(|h| (*h).to_owned()).collect();
        let e = MappingPlan::build(
            &meta,
            "sales_invoice",
            &children,
            &[],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap_err();
        assert!(
            matches!(e, ImportError::Meta { .. }),
            "duplicate child binding must fail: {e:?}"
        );
    }

    /// Group-key binds only by override; two keys are an error.
    #[test]
    fn group_key_binds_by_override_only() {
        let meta = testmeta::meta();
        let headers: Vec<String> = ["ref", "number"].iter().map(|h| (*h).to_owned()).collect();
        let p = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[Override::new("ref", "group_key")],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap();
        assert_eq!(p.group_key(), Some(0));
        assert_eq!(
            p.target(0),
            Some(&Target::GroupKey),
            "the key column is bound as the key"
        );
        assert!(
            !matches!(p.target(1), Some(Target::GroupKey)),
            "the second column kept its auto-match"
        );

        let e = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[
                Override::new("ref", "group_key"),
                Override::new("number", "group_key"),
            ],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap_err();
        assert!(matches!(e, ImportError::Meta { .. }), "{e:?}");
    }

    /// An unknown doctype fails before any row.
    #[test]
    fn unknown_doctype_fails_the_build() {
        let meta = testmeta::meta();
        let headers = vec!["a".to_owned()];
        let e = MappingPlan::build(
            &meta,
            "no_such_doctype",
            &headers,
            &[],
            ConversionOptions::default(),
            formats(),
        )
        .unwrap_err();
        assert_eq!(e.code(), crate::error::UNKNOWN_DOCTYPE);
    }
}
