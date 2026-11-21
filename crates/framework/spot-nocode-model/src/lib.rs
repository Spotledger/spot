//! `spot-nocode-model` — the server-side model of the no-code DocType and form designer
//! (WP-DX-06.T1).
//!
//! A business user builds a DocType and its form without code. The machinery for runtime
//! DocTypes already exists (`spot-customise`, `spot-views`); this crate is the **draft,
//! validate, preview, publish** layer that makes it safe for a non-developer:
//!
//! - [`Draft`] holds a DocType, its fields, its form layout and its child tables. Every change
//!   is a pure [`DraftOp`] applied by [`apply_op`], and [`DraftEditor`] keeps the op log so
//!   undo and redo are exact.
//! - [`validate`] compiles the draft to the effective metadata the platform runs and turns
//!   every registry and view error into a plain-language [`FriendlyIssue`] with a fix
//!   suggestion and the element path, keeping the underlying `meta.*`/`view.*` code.
//! - [`plan_type_change`] classifies a type change as safe, lossy or refused and states what
//!   happens to the existing values.
//! - [`preview`] compiles the draft into an ephemeral metadata overlay and form layout the
//!   client renders with sample data; nothing is persisted.
//! - [`plan_publish`] builds the `spot-customise` records, the additive schema plan and the
//!   risk class, records provenance, and [`publish`] applies it through a [`Publisher`] after
//!   checking the `designer.publish` permission.
//! - Guard rails (reserved names, field and depth limits, no script, no raw SQL, protected
//!   system DocTypes, and the data-loss confirmation token for a removal) live in [`draft`].
//!
//! # What this crate does not do
//!
//! It never applies a schema change itself (only the migration plan path does), never reads a
//! clock (time is an argument), never uses floats for money or quantities, and contains no
//! `unsafe`. Every error carries a stable `nocode_model.*` code catalogued in
//! `crates/framework/spot-errors/catalogue/nocode_model.toml`.

#![forbid(unsafe_code)]

pub mod codes;
pub mod condition;
pub mod draft;
pub mod layout;
pub mod naming;
mod ops;
pub mod preview;
pub mod publish;
pub mod site;
pub mod typechange;
pub mod validate;

pub use condition::{Condition, ConditionError, Join, Operator, Row, Value};
pub use draft::{
    ChildDraft, Draft, DraftEditor, DraftOp, DraftStatus, MAX_CHILD_DEPTH, MAX_FIELDS,
    MAX_SECTIONS, OpError, PermissionsTemplate, apply_op, data_loss_token, is_protected_doctype,
    is_reserved_field, looks_like_script, looks_like_sql,
};
pub use layout::FormLayout;
pub use naming::{
    CUSTOM_PREFIX, doctype_name, field_name, is_storage_name, safe_identifier, unique_name,
};
pub use ops::naming_is_valid;
pub use preview::{
    ChildPreview, PreviewField, PreviewMetadata, preview, preview_in, preview_with, sample_value,
};
pub use publish::{
    Designer, MemoryPublisher, PUBLISH_PERMISSION, Provenance, PublishError, PublishPlan,
    PublishReceipt, Publisher, plan_publish, plan_publish_as, publish,
};
pub use site::SiteContext;
pub use typechange::{Classification, SampleStats, TypeChangePlan, plan_type_change};
pub use validate::{Element, FriendlyIssue, Severity, ValidationReport, validate};
