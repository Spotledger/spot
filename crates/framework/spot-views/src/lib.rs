//! `spot-views`: the view definition model, the layer merge and the validation
//! (WP-UI-10.T1; ADR-068 §7, ADR-069).
//!
//! Where fields appear, in what order and how they are grouped is the view's business, never
//! the field's (ADR-068 §7). A view is **layers of differences merged over the effective
//! metadata at resolution time**, so a layout change needs no rebuild and an
//! app update that adds a field still shows it to a site that customised the layout.
//!
//! The pieces:
//! - [`ViewDef`]: one view per DocType and kind (`form`, `list`, `grid`): tabs, sections and
//!   [`Item`]s (a field, an unbound control, or a nested group), list columns and default
//!   sort, grid columns, a density hint and actions. TOML in and out with stable key order;
//!   errors carry a position and a stable `view.*` code and are reported all at once.
//! - [`ViewLayer`]: a **diff** over a base view (move/hide/show a field, add an item or a
//!   tab, rename a section, set columns, set a hint), with a specificity
//!   [`LayerKind`](layer::LayerKind) (`standard` < `site` < `role` < `step`).
//! - [`resolve`]: applies the layers least to most specific; a diff naming a field that no
//!   longer exists is dropped with a warning in the result, never an error; fields nobody
//!   mentioned are appended in definition order into the last section of a form view, unless
//!   a layer explicitly hid them. Pure and deterministic.
//! - [`roles`]: the one-role-view rule of ADR-068 §7.2 — the user's highest-priority role
//!   that has a view wins; role views are not merged.
//! - [`validate`]: the save-time checks (ADR-068 §7.5) — hiding a `required` field with no
//!   `default` cannot strand a save.
//! - [`generate`]: the generated default views (WP-UI-10.T2, ADR-068 §7) — one pure call
//!   per DocType and kind that turns the effective metadata (the definition's `fields`
//!   order) into a form, list or grid [`ViewDef`], so a DocType created at runtime has a
//!   usable view with no code. This is the base the layers apply over.
//!
//! Renderer-neutral by design: no rendering, no Avalonia, no .NET. Controls are referred to
//! by catalog name (strings); the control catalog itself is not this task.

pub mod error;
/// The generated default view entry point and its constants.
pub mod generate;
pub mod layer;
pub mod resolve;
pub mod roles;
pub mod validate;
pub mod viewdef;

pub use error::{Position, ViewError, ViewErrorKind};
pub use generate::{
    CUSTOM_PREFIX, CUSTOM_SECTION, CUSTOM_SECTION_TITLE, ENGINE_FIELDS, FIELDS_SECTION,
    LIST_EXTRA_COLUMNS, STATUS_SECTION, STATUS_SECTION_TITLE, TABLE_SECTION_PREFIX, default_view,
    humanize::humanize_label,
};
pub use layer::{LayerKind, LayerName, Op, ViewAnchor, ViewLayer};
pub use resolve::{ResolveWarning, ResolvedView, resolve, resolve_over};
pub use roles::{RoleRef, select_role_view};
pub use validate::validate_layer;
pub use viewdef::{Action, Column, Density, Item, Sort, Tab, ViewDef, ViewKind};
