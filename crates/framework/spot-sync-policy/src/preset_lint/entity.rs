//! Lints L3 (optional exclusions), L4 (scope fields), L5 (child policy, derived entities) and
//! L6 (unique keys): the findings that need one entity and its own fields only.

use std::collections::BTreeSet;

use spot_meta::{Definition, FieldType};

use super::extras::KeyScope;
use super::{Ctx, Finding, codes};
use crate::preset::{Preset, PresetPolicy, Selector};

pub(crate) fn lint_entities(ctx: &Ctx<'_>, out: &mut Vec<Finding>) {
    child_and_derived(ctx, out);
    for root in ctx.roots() {
        let Some(policy) = ctx.table.get(&root.name) else {
            continue;
        };
        if !policy.preset.is_synced() {
            continue;
        }
        excluded_fields(root, policy, out);
        selectors(ctx, root, policy, out);
        unique_keys(ctx, root, policy.preset, out);
    }
}

/// L5: children follow their parent, and derived data is rebuilt, not copied.
fn child_and_derived(ctx: &Ctx<'_>, out: &mut Vec<Finding>) {
    for (name, policy) in ctx.table.iter() {
        let Some(definition) = ctx.defs.get(name) else {
            continue;
        };
        if !policy.preset.is_synced() {
            continue;
        }
        if definition.is_child {
            out.push(Finding::new(
                codes::LINT_CHILD_POLICY,
                name,
                format!(
                    "child table `{name}` declares the {} policy of its own; a child follows \
                     its parent's policy, so remove the declaration",
                    policy.preset
                ),
                &[],
            ));
        }
        if ctx.extras.is_derived(name) {
            out.push(Finding::new(
                codes::LINT_DERIVED_SYNCED,
                name,
                format!(
                    "`{name}` is derived data and has the {} policy; the destination rebuilds \
                     it from the records that sync, so make it `local`",
                    policy.preset
                ),
                &[],
            ));
        }
    }
}

/// L3: an excluded field is missing at the destination, so the destination needs a value for it
/// unless the field is optional or has a default.
fn excluded_fields(root: &Definition, policy: &PresetPolicy, out: &mut Vec<Finding>) {
    let mut seen = BTreeSet::new();
    for name in policy.excluded_fields.iter_all() {
        if !seen.insert(name.as_str().to_owned()) {
            continue;
        }
        match root.fields.iter().find(|f| f.name == name.as_str()) {
            None => out.push(Finding::new(
                codes::LINT_EXCLUDED_UNKNOWN_FIELD,
                &root.name,
                format!(
                    "excluded field `{name}` is not a field of `{}`, so it excludes nothing",
                    root.name
                ),
                &[],
            )),
            Some(spec) if spec.required && spec.default.is_none() => out.push(Finding::new(
                codes::LINT_EXCLUDED_REQUIRED,
                &root.name,
                format!(
                    "excluded field `{name}` is required and has no default; the destination \
                     stores full documents and needs a value for every declared field"
                ),
                &[],
            )),
            Some(_) => {}
        }
    }
}

/// L4: every field a selector or `owner_key` reads must be able to name a key.
fn selectors(ctx: &Ctx<'_>, root: &Definition, policy: &PresetPolicy, out: &mut Vec<Finding>) {
    let mut reads: Vec<(&'static str, String)> = Vec::new();
    for (setting, selector) in [("scope", &policy.scope), ("relay", &policy.relay)] {
        if let Some(Selector::Key { field, .. }) = selector {
            reads.push((setting, field.as_str().to_owned()));
        }
    }
    if let Some(key) = &policy.owner_key {
        reads.push(("owner_key", key.field.as_str().to_owned()));
    }
    for (setting, field) in reads {
        let problem = match root.fields.iter().find(|f| f.name == field) {
            None => Some("is not a field of the entity".to_owned()),
            Some(spec) => {
                let can_name_a_key = matches!(
                    spec.ty,
                    FieldType::Link | FieldType::Select | FieldType::TableMultiselect
                ) || ctx.extras.is_key_typed(&root.name, &field);
                (!can_name_a_key).then(|| {
                    format!(
                        "is a {} field, which cannot name a key; use a link, an enum or a \
                         key-typed field",
                        format!("{:?}", spec.ty).to_ascii_lowercase()
                    )
                })
            }
        };
        if let Some(reason) = problem {
            out.push(Finding::new(
                codes::LINT_SCOPE_FIELD,
                &root.name,
                format!("the `{setting}` setting reads field `{field}`, which {reason}"),
                &[],
            ));
        }
    }
}

/// L6: a unique key that two nodes can create the same value for collides at the hub.
fn unique_keys(ctx: &Ctx<'_>, root: &Definition, preset: Preset, out: &mut Vec<Finding>) {
    if !matches!(preset, Preset::Record | Preset::Handover) {
        return;
    }
    let mut keys: BTreeSet<&str> = root
        .fields
        .iter()
        .filter(|f| f.unique)
        .map(|f| f.name.as_str())
        .collect();
    if let Some(field) = root
        .naming
        .as_deref()
        .and_then(|n| n.strip_prefix("field:"))
    {
        keys.insert(field);
    }
    for field in keys {
        if ctx.extras.key_scope(&root.name, field) == KeyScope::Global {
            out.push(Finding::new(
                codes::LINT_UNIQUE_KEY,
                &root.name,
                format!(
                    "`{field}` is a unique key on a {preset} entity that is neither scoped by \
                     origin nor generated from a per-node series, so two nodes can create the \
                     same value"
                ),
                &[],
            ));
        }
    }
}
