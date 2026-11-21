//! The generated default views: one pure call per DocType and kind, from the effective
//! metadata only (ADR-068 §7; brief WP-UI-10.T2 deliverables 1–4).

pub mod columns;
pub mod humanize;
pub(crate) mod layout;

use spot_meta::EffectiveMeta;

use crate::error::{ViewError, ViewErrorKind};
use crate::viewdef::{ViewDef, ViewKind};

/// How many columns the generated list shows beyond the title field (brief deliverable 2).
pub const LIST_EXTRA_COLUMNS: usize = 5;

/// The engine-managed fields of a posted DocType (`doc_state`, `amended_from`); the
/// registry appends them to every effective posted definition, in this order.
pub const ENGINE_FIELDS: [&str; 2] = ["doc_state", "amended_from"];

/// The storage-name prefix spot-meta requires of custom fields.
pub const CUSTOM_PREFIX: &str = "custom_";

/// The `fields` section id (the scalar fields of a generated form).
pub const FIELDS_SECTION: &str = "fields";
/// A table section's id prefix: `table_<field>` holds the `table` field `<field>`.
pub const TABLE_SECTION_PREFIX: &str = "table_";
/// The collapsed Status section id (read-only and engine-managed fields).
pub const STATUS_SECTION: &str = "status";
/// The Status section's title.
pub const STATUS_SECTION_TITLE: &str = "Status";
/// The Custom section id (the site's `custom_` fields).
pub const CUSTOM_SECTION: &str = "custom";
/// The Custom section's title.
pub const CUSTOM_SECTION_TITLE: &str = "Custom";

/// Generates the default view of `doctype` with `kind` over the effective metadata
/// (ADR-068 §7: the `fields` order of the definition is the order). Pure and
/// deterministic: the same inputs give a byte-equal TOML [`ViewDef`] every time, so a
/// generated default view is stable text — diffable, cacheable, golden-testable.
///
/// The customisation layers apply over this base: a site layer moves, hides or regroups
/// what the default put where ([`resolve`](crate::resolve)).
///
/// # Errors
/// A [`Vec<ViewError>`] with one error when:
/// - the DocType is not in the metadata (`view.unknown_field`);
/// - `kind` is [`ViewKind::Grid`] over a DocType that is not a child (`view.bad_shape`):
///   the grid is the child-table shape, so a grid over a parent document has no meaning.
pub fn default_view(
    meta: &EffectiveMeta,
    doctype: &str,
    kind: ViewKind,
) -> Result<ViewDef, Vec<ViewError>> {
    let Some(definition) = meta.definition(doctype) else {
        return Err(vec![ViewError::new(
            ViewErrorKind::UnknownField,
            format!("the DocType '{doctype}' is not in the metadata; there is no default view"),
        )]);
    };
    if kind == ViewKind::Grid && !effective::is_child(meta, doctype) {
        return Err(vec![ViewError::new(
            ViewErrorKind::BadShape,
            format!("the DocType '{doctype}' is not a child; a grid view is the child-table shape"),
        )]);
    }
    let view = match kind {
        ViewKind::Form => layout::form_view(definition),
        ViewKind::List => columns::list_view(definition),
        ViewKind::Grid => columns::grid_view(definition),
    };
    Ok(view)
}

/// Reads against `spot_meta`'s pub(crate)-free surface: the flags the shapes need are on
/// [`Definition`] itself, so this module only re-names them.
mod effective {
    /// Whether `doctype` is a child DocType, from its effective definition.
    pub(crate) fn is_child(meta: &spot_meta::EffectiveMeta, doctype: &str) -> bool {
        meta.definition(doctype).is_some_and(|d| d.is_child)
    }
}
