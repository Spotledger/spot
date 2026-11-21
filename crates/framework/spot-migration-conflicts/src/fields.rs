//! Custom fields and property overrides against the new metadata.
//!
//! Two checks:
//! - a custom field whose name a new **standard** field now uses: the merge would put two fields
//!   of one name on the DocType, so the release cannot go out until the site renames or drops it;
//! - a property override whose target the release removes or renames, or whose `options` list no
//!   longer matches the field's new option list (the release added a value the override lacks, or
//!   the override keeps a value the release removed).

use std::collections::BTreeSet;

use spot_meta::{Definition, Property};

use crate::collect::Collector;
use crate::input::ReleaseContext;
use crate::model::{ArtefactKind, ArtefactRef, Cause, ConflictCategory};
use crate::release::ReleaseDelta;

/// Runs the custom-field and property-override checks.
pub(crate) fn check(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    custom_fields(ctx, delta, out);
    overrides(ctx, delta, out);
}

/// The custom-field checks.
fn custom_fields(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    for custom in ctx.customisations.fields() {
        let doctype = custom.doctype.as_str();
        let name = custom.spec.name.as_str();
        let artefact = ArtefactRef::new(ArtefactKind::CustomField, format!("{doctype}.{name}"));
        if let Some((cause, resolution)) = delta.doctype_cause(doctype) {
            out.push(
                ConflictCategory::CustomFieldDocTypeRemoved,
                cause,
                artefact,
                resolution,
            );
            continue;
        }
        if standard_has_field(ctx.new_standard, doctype, name) {
            let cause = match delta.field_added(doctype, name) {
                Some(change) => Cause::new(
                    change,
                    format!(
                        "the release adds the standard field `{doctype}.{name}`, which the site's custom field already uses"
                    ),
                ),
                None => Cause::new(
                    format!("release/metadata/{doctype}/field_added/{name}"),
                    format!(
                        "the new standard metadata declares the field `{doctype}.{name}`, which the site's custom field already uses"
                    ),
                ),
            };
            out.push(
                ConflictCategory::CustomFieldCollision,
                cause,
                artefact,
                "Rename the custom field, or remove it if the new standard field replaces it.",
            );
        }
    }
}

/// The property-override checks.
fn overrides(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    for over in ctx.customisations.overrides() {
        let doctype = over.doctype.as_str();
        let key = over.property.key();
        let field = over.field.as_deref();
        let artefact = ArtefactRef::new(
            ArtefactKind::PropertyOverride,
            format!("{doctype}.{}#{key}", field.unwrap_or("*")),
        );
        if let Some((cause, resolution)) = delta.doctype_cause(doctype) {
            out.push(
                ConflictCategory::OverrideDocTypeRemoved,
                cause,
                artefact,
                resolution,
            );
            continue;
        }
        if let Some(field) = field
            && let Some((cause, resolution)) = delta.field_cause(doctype, field)
        {
            out.push(
                ConflictCategory::OverrideFieldRemoved,
                cause,
                artefact,
                resolution,
            );
            continue;
        }
        if let (Some(field), Property::Options(values)) = (field, &over.property)
            && let Some((cause, resolution)) = options_conflict(ctx, doctype, field, values)
        {
            out.push(
                ConflictCategory::OverrideOptionsConflict,
                cause,
                artefact,
                resolution,
            );
        }
    }
}

/// Whether the new standard definitions declare `doctype.field`.
fn standard_has_field(new_standard: &[Definition], doctype: &str, field: &str) -> bool {
    new_standard
        .iter()
        .find(|d| d.name == doctype)
        .is_some_and(|d| d.fields.iter().any(|f| f.name == field))
}

/// Whether a site `options` override still matches the field's new standard option list.
///
/// The override replaces the field's whole option list and may only add to it, so a value the
/// release added and the override lacks is refused at merge: that is always a conflict. When the
/// caller supplied the pre-release definitions, a value the release **retired** (in the old
/// standard, gone from the new) that the override still lists is also a conflict: the override
/// resurrects an option the release removed. A value the site itself added is never a conflict.
fn options_conflict(
    ctx: &ReleaseContext<'_>,
    doctype: &str,
    field: &str,
    override_values: &[String],
) -> Option<(Cause, String)> {
    let new_values = select_values(ctx.new_standard, doctype, field)?;
    let chosen: BTreeSet<&String> = override_values.iter().collect();
    let missing: Vec<String> = new_values
        .iter()
        .filter(|v| !chosen.contains(v))
        .cloned()
        .collect();

    let mut retired: Vec<String> = Vec::new();
    if let Some(old_standard) = ctx.old_standard
        && let Some(old_values) = select_values(old_standard, doctype, field)
    {
        retired = old_values
            .iter()
            .filter(|v| !new_values.contains(v) && chosen.contains(v))
            .cloned()
            .collect();
    }

    if missing.is_empty() && retired.is_empty() {
        return None;
    }
    let change = format!("release/metadata/{doctype}/options/{field}");
    let effect = format!(
        "the option list of `{doctype}.{field}` changes and the site's `options` override no longer matches"
    );
    let mut steps: Vec<String> = Vec::new();
    if !missing.is_empty() {
        steps.push(format!("add the new value(s) {}", quote_list(&missing)));
    }
    if !retired.is_empty() {
        steps.push(format!(
            "drop the retired value(s) {}",
            quote_list(&retired)
        ));
    }
    Some((
        Cause::new(change, effect),
        format!("Update the `options` override to {}.", steps.join(", and ")),
    ))
}

/// The option values of a select field in `definitions`, when the field is there.
fn select_values(definitions: &[Definition], doctype: &str, field: &str) -> Option<Vec<String>> {
    let definition = definitions.iter().find(|d| d.name == doctype)?;
    let spec = definition.fields.iter().find(|f| f.name == field)?;
    Some(spec.select_values())
}

/// A comma-separated list of quoted values, in the order given.
fn quote_list(values: &[String]) -> String {
    values
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
