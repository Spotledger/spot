// Preflight: report unmapped required fields and unused columns before any row is processed.

use spot_meta::{EffectiveMeta, FieldType};

use crate::error::ImportError;
#[doc(inline)]
pub use crate::mapping::{ImportPreflight, UnmappedField};
use crate::mapping::{MappingPlan, Target};
use std::collections::BTreeMap;

/// Preflight: the DocType fields no column feeds and the columns nothing claims, checked
/// against the plan's own bindings (brief item 2: "reported before any row is processed").
pub fn preflight(plan: &MappingPlan, meta: &EffectiveMeta) -> ImportPreflight {
    let mut report = ImportPreflight {
        group_key_column: plan.group_key(),
        ..ImportPreflight::default()
    };
    report.errors.extend(plan_validate(plan, meta));
    let Some(def) = meta.definition(&plan.doctype) else {
        return report;
    };
    // Build the bound-field set per table.
    let mut bound_parent: Vec<String> = Vec::new();
    let mut bound_child: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in plan.columns() {
        match t.target {
            Target::Field(f) => bound_parent.push(f),
            Target::Child(t, f) => bound_child.entry(t).or_default().push(f),
            Target::GroupKey => {}
        }
    }
    for f in &def.fields {
        match f.ty {
            ty if ty.is_table() => {
                let child_name = f.options.as_deref().unwrap_or("");
                let Some(child_def) = meta.definition(child_name) else {
                    continue;
                };
                let fed = bound_child
                    .get(f.name.as_str())
                    .map_or(&[][..], Vec::as_slice);
                for cf in &child_def.fields {
                    if cf.ty.is_table() || cf.name == "doc_state" {
                        continue;
                    }
                    if !fed.iter().any(|name: &String| name == &cf.name) {
                        report.unmapped_fields.push(UnmappedField {
                            name: cf.name.clone(),
                            label: cf.label_or_name().to_owned(),
                            required: cf.required,
                            child: true,
                        });
                    }
                }
            }
            _ if f.name == "doc_state" || f.ty == FieldType::Secret => {}
            _ => {
                if !bound_parent.iter().any(|name: &String| name == &f.name) {
                    report.unmapped_fields.push(UnmappedField {
                        name: f.name.clone(),
                        label: f.label_or_name().to_owned(),
                        required: f.required,
                        child: false,
                    });
                }
            }
        }
    }
    for (index, header) in plan.headers().iter().enumerate() {
        if plan.target(index).is_none() {
            report.unmapped_columns.push((index, header.clone()));
        }
    }
    report
}

/// What preflight reports before the import can run: an unresolvable plan is a fatal error,
/// not a per-row one.
pub fn blocking_errors(plan: &MappingPlan, meta: &EffectiveMeta) -> Vec<ImportError> {
    plan_validate(plan, meta)
}

/// Plan-shape checks that fail the import before any row: a mapped field that the
/// DocType does not declare, a child mapping for a table the DocType does not declare.
fn plan_validate(plan: &MappingPlan, meta: &EffectiveMeta) -> Vec<ImportError> {
    let mut errors = Vec::new();
    let Some(def) = meta.definition(&plan.doctype) else {
        // `build` refused unknown doctypes already; nothing to add here.
        return errors;
    };
    for mapping in plan.columns() {
        match &mapping.target {
            Target::GroupKey => {}
            Target::Field(name) => {
                if !def
                    .fields
                    .iter()
                    .any(|f| f.name == *name && !f.ty.is_table())
                {
                    errors.push(ImportError::Meta {
                        message: format!(
                            "column {} (`{}`) is mapped to `{name}`, which `{}` does not declare as a field",
                            mapping.index, mapping.header, plan.doctype
                        ),
                    });
                }
            }
            Target::Child(table, field) => {
                let table_field = def.fields.iter().find(|f| f.name == *table);
                let Some(table_field) = table_field else {
                    errors.push(ImportError::Meta {
                        message: format!(
                            "column {} (`{}`) is mapped to the table `{table}`, which `{}` does not declare",
                            mapping.index, mapping.header, plan.doctype
                        ),
                    });
                    continue;
                };
                let child = table_field.options.as_deref().unwrap_or("");
                match meta.definition(child) {
                    None => errors.push(ImportError::Meta {
                        message: format!(
                            "the table `{table}` uses the DocType `{child}`, which the metadata does not define"
                        ),
                    }),
                    Some(child_def) if !child_def.fields.iter().any(|f| f.name == *field) => {
                        errors.push(ImportError::Meta {
                            message: format!(
                                "column {} (`{}`) is mapped to `{table}.{field}`, but `{child}` does not declare it",
                                mapping.index, mapping.header
                            ),
                        });
                    }
                    Some(_) => {}
                }
            }
        }
    }
    errors
}
