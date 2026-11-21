//! The stable `nocode_builders.*` codes this crate raises, in one place.
//!
//! Every code is catalogued in `crates/framework/spot-errors/catalogue/nocode_builders.toml`:
//! the catalogue carries the plain-language summary and the fix suggestion a business user
//! reads. The codes of the layers underneath (`report.*`, `bpm_model.*`) are **kept** beside
//! the friendly code on every [`Issue`](crate::Issue) rather than re-coded here: this module
//! names only the codes this crate raises itself.

/// A report, dashboard, field or approver name is not a valid name.
pub const BAD_NAME: &str = "nocode_builders.bad_name";
/// A report or dashboard has nothing to show yet.
pub const EMPTY: &str = "nocode_builders.empty";
/// Two columns, tiles, parameters or joins share a name.
pub const DUPLICATE: &str = "nocode_builders.duplicate";
/// A field the draft names does not exist on its record type.
pub const UNKNOWN_FIELD: &str = "nocode_builders.unknown_field";
/// A field path names a link that is not declared, or a link that cannot be followed.
pub const UNKNOWN_JOIN: &str = "nocode_builders.unknown_join";
/// A column, total or chart series names a column the report does not have.
pub const UNKNOWN_COLUMN: &str = "nocode_builders.unknown_column";
/// A total, chart or parameter does not fit the kind of field it uses.
pub const BAD_TYPE: &str = "nocode_builders.bad_type";
/// A setting has a value the builder does not accept (an empty title, a limit out of range,
/// a required parameter with a default).
pub const BAD_VALUE: &str = "nocode_builders.bad_value";
/// A filter or tile names a value the report or dashboard does not offer.
pub const UNKNOWN_PARAMETER: &str = "nocode_builders.unknown_parameter";
/// A chart asks for a shape its columns cannot draw.
pub const BAD_CHART: &str = "nocode_builders.bad_chart";
/// The author may not read and report on a field, so the builder may not offer it.
pub const FIELD_NOT_OFFERED: &str = "nocode_builders.field_not_offered";
/// A tile does not fit the dashboard's grid or overlaps another tile.
pub const BAD_LAYOUT: &str = "nocode_builders.bad_layout";
/// A tile names a report the site does not have.
pub const UNKNOWN_REPORT: &str = "nocode_builders.unknown_report";
/// A dashboard's refresh policy is not one the platform accepts.
pub const BAD_REFRESH: &str = "nocode_builders.bad_refresh";
/// The approval template names a record type the site does not have.
pub const UNKNOWN_DOCTYPE: &str = "nocode_builders.unknown_doctype";
/// An approver is empty or cannot be resolved.
pub const BAD_APPROVER: &str = "nocode_builders.bad_approver";
/// The built approval process does not pass BPM validation.
pub const INVALID_PROCESS: &str = "nocode_builders.invalid_process";
/// The compiled report exceeds an ADR-069 limit.
pub const TOO_LARGE: &str = "nocode_builders.too_large";
/// A preview budget is zero, so the preview would run unrestricted.
pub const PREVIEW_BUDGET: &str = "nocode_builders.preview_budget";
/// A preview took longer than its time budget allows.
pub const PREVIEW_TOO_SLOW: &str = "nocode_builders.preview_too_slow";
/// The draft is not ready to publish.
pub const NOT_READY: &str = "nocode_builders.not_ready";
/// The caller does not hold the `designer.publish` permission.
pub const PUBLISH_FORBIDDEN: &str = "nocode_builders.publish_forbidden";
/// The sink a publish writes to refused the artifact.
pub const PUBLISH_FAILED: &str = "nocode_builders.publish_failed";
/// The op is not supported for the draft it names.
pub const UNSUPPORTED_OP: &str = "nocode_builders.unsupported_op";
