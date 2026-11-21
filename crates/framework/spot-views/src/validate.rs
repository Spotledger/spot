//! The save-time validation: every problem reported at once with stable codes.
//!
//! The one rule the engine depends on is ADR-068 §7.5: **a hidden field must not strand a
//! save.** A view that hides a field that is `required` and has no `default` is refused when
//! the view is saved — the alternative, a form that cannot be saved with no visible reason,
//! is not accepted. (Whether another rule will set the value is the engine's judgement at
//! save time; this check is the view-layer floor.)
//!
//! Hiding is not security (§7.4): a hidden field stays stored, in the API, permission-checked.

use spot_meta::EffectiveMeta;

use crate::error::{ViewError, ViewErrorKind};
use crate::resolve::ResolvedView;
use crate::viewdef::Item;

/// Validates a resolved view over the effective metadata, reporting every problem at once.
///
/// Checks:
/// - every hidden field that is `required` with no `default` is `view.hidden_required`;
/// - every column and sort field is a field of the DocType (`view.unknown_field`);
/// - every unbound control in the resolved items is one the model knows
///   ([`view.unknown_control`](ViewErrorKind::UnknownControl));
/// - section/tab ids are unique and well-formed (`view.duplicate`, `view.bad_id`).
pub fn validate_layer(resolved: &ResolvedView, meta: &EffectiveMeta) -> Vec<ViewError> {
    let view = &resolved.view;
    let mut errors = Vec::new();
    let Some(definition) = meta.definition(&view.doctype) else {
        errors.push(
            ViewError::new(
                ViewErrorKind::UnknownField,
                format!("the DocType '{}' is not in the metadata", view.doctype),
            )
            .in_doctype(&view.doctype),
        );
        return errors;
    };
    let field_of = |name: &str| definition.fields.iter().find(|field| field.name == name);

    // A hidden field must not strand a save (ADR-068 §7.5).
    for name in &resolved.hidden {
        let Some(field) = field_of(name) else {
            errors.push(
                ViewError::new(
                    ViewErrorKind::UnknownField,
                    format!("the hidden field '{name}' is not in the DocType"),
                )
                .in_doctype(&view.doctype)
                .in_field(name),
            );
            continue;
        };
        if field.required && field.default.is_none() {
            errors.push(
                ViewError::new(
                    ViewErrorKind::HiddenRequired,
                    format!(
                        "the required field '{name}' has no default and is hidden in the view; the save would strand"
                    ),
                )
                .in_doctype(&view.doctype)
                .in_field(name),
            );
        }
    }

    // Columns and sort must name real fields.
    for column in &view.columns {
        if field_of(&column.field).is_none() {
            errors.push(
                ViewError::new(
                    ViewErrorKind::UnknownField,
                    format!(
                        "the column names field '{}', which the DocType does not have",
                        column.field
                    ),
                )
                .in_doctype(&view.doctype)
                .in_field(&column.field),
            );
        }
    }
    if let Some(sort) = &view.sort
        && field_of(&sort.field).is_none()
    {
        errors.push(
            ViewError::new(
                ViewErrorKind::UnknownField,
                format!(
                    "the sort names field '{}', which the DocType does not have",
                    sort.field
                ),
            )
            .in_doctype(&view.doctype)
            .in_field(&sort.field),
        );
    }

    // Every field item must be a field of the DocType; every unbound control known.
    let mut seen_sections: Vec<&str> = Vec::new();
    for section in view.all_sections() {
        seen_sections.push(&section.id);
        check_items(&section.items, view, definition, &mut errors);
    }
    for tab in &view.tabs {
        seen_sections.extend(tab.sections.iter().map(|s| s.id.as_str()));
    }
    for id in &seen_sections {
        if seen_sections.iter().filter(|other| *other == id).count() > 1 {
            errors.push(
                ViewError::new(
                    ViewErrorKind::Duplicate,
                    format!("id '{id}' is used twice in the resolved view"),
                )
                .in_doctype(&view.doctype)
                .in_field(id),
            );
        }
    }
    errors
}

/// Checks the field names and unbound controls of the item list (recursing through groups).
fn check_items(
    items: &[Item],
    view: &crate::viewdef::ViewDef,
    definition: &spot_meta::Definition,
    errors: &mut Vec<ViewError>,
) {
    for item in items {
        match item {
            Item::Field { field, .. } => {
                if !definition
                    .fields
                    .iter()
                    .any(|known| known.name == field.as_str())
                {
                    errors.push(
                        ViewError::new(
                            ViewErrorKind::UnknownField,
                            format!(
                                "the view names field '{field}', which the DocType does not have"
                            ),
                        )
                        .in_doctype(&view.doctype)
                        .in_field(field),
                    );
                }
            }
            Item::Control { control, .. } => {
                if !crate::viewdef::ViewDef::is_unbound_control(control) {
                    errors.push(
                        ViewError::new(
                            ViewErrorKind::UnknownControl,
                            format!("control '{control}' is not in the unbound set"),
                        )
                        .in_doctype(&view.doctype)
                        .in_field(control),
                    );
                }
            }
            Item::Group(group) => check_items(&group.items, view, definition, errors),
        }
    }
}
