//! The stable `nocode_model.*` codes this crate raises, in one place.
//!
//! Every code is catalogued in `crates/framework/spot-errors/catalogue/nocode_model.toml`:
//! the catalogue carries the plain-language summary and the fix suggestion a designer sees.
//! The registry and view codes the draft compiles to (`meta.*`, `view.*`) are **kept** beside
//! the friendly code on every [`FriendlyIssue`](crate::FriendlyIssue) rather than re-coded
//! here: this module names only the codes this crate raises itself.

/// A field or DocType name is reserved by the platform.
pub const RESERVED_NAME: &str = "nocode_model.reserved_name";
/// A field name is not a valid storage name.
pub const BAD_NAME: &str = "nocode_model.bad_name";
/// The DocType has more fields than the platform allows.
pub const MAX_FIELDS: &str = "nocode_model.max_fields";
/// The form or child nesting is deeper than the platform allows.
pub const MAX_DEPTH: &str = "nocode_model.max_depth";
/// A default, option or rule carries something that looks like a script.
pub const NO_SCRIPT: &str = "nocode_model.no_script";
/// A default, option or rule carries something that looks like a raw SQL statement.
pub const NO_SQL: &str = "nocode_model.no_sql";
/// A protected system DocType cannot be edited by the designer.
pub const PROTECTED_DOCTYPE: &str = "nocode_model.protected_doctype";
/// Removing a field needs the explicit data-loss confirmation token.
pub const NEEDS_DATA_LOSS_CONFIRMATION: &str = "nocode_model.needs_data_loss_confirmation";
/// The draft names a field it does not have.
pub const UNKNOWN_FIELD: &str = "nocode_model.unknown_field";
/// The draft names a child DocType it does not have.
pub const UNKNOWN_CHILD: &str = "nocode_model.unknown_child";
/// Two fields share a name.
pub const DUPLICATE_FIELD: &str = "nocode_model.duplicate_field";
/// Two layout sections share an id.
pub const DUPLICATE_SECTION: &str = "nocode_model.duplicate_section";
/// Two layout tabs share an id.
pub const DUPLICATE_TAB: &str = "nocode_model.duplicate_tab";
/// The layout names a section it does not declare.
pub const UNKNOWN_SECTION: &str = "nocode_model.unknown_section";
/// The layout names a tab it does not declare.
pub const UNKNOWN_TAB: &str = "nocode_model.unknown_tab";
/// The layout cannot take the shape asked of it (tabs and top-level sections together).
pub const BAD_LAYOUT: &str = "nocode_model.bad_layout";
/// A friendly condition did not compile to the shared expression language.
pub const BAD_CONDITION: &str = "nocode_model.bad_condition";
/// The naming series pattern is not one the platform accepts.
pub const BAD_NAMING: &str = "nocode_model.bad_naming";
/// A link or child table names a target that cannot be used.
pub const BAD_TARGET: &str = "nocode_model.bad_target";
/// A field's type change would lose or corrupt data, so it is refused.
pub const TYPE_CHANGE_REFUSED: &str = "nocode_model.type_change_refused";
/// A field's type change is lossy for some existing values; the designer is warned.
pub const TYPE_CHANGE_LOSSY: &str = "nocode_model.type_change_lossy";
/// The caller does not hold the `designer.publish` permission.
pub const PUBLISH_FORBIDDEN: &str = "nocode_model.publish_forbidden";
/// The publisher refused the plan, or the store it writes to failed.
pub const PUBLISH_FAILED: &str = "nocode_model.publish_failed";
/// The draft is not valid, so it cannot be published yet.
pub const NOT_READY: &str = "nocode_model.not_ready";
/// A field added to an existing DocType must be a `custom_` field.
pub const EXISTING_NEEDS_PREFIX: &str = "nocode_model.existing_needs_prefix";
/// Only a custom field may be removed from an existing DocType.
pub const NOT_CUSTOM_FIELD: &str = "nocode_model.not_custom_field";
/// The draft op is not supported for the target it names.
pub const UNSUPPORTED_OP: &str = "nocode_model.unsupported_op";
/// A select field's option list is empty or repeats a value.
pub const BAD_OPTIONS: &str = "nocode_model.bad_options";
/// A field is still referred to by another field, so it cannot be removed.
pub const REFERENCED: &str = "nocode_model.referenced";

/// The friendly code for a metadata registry error kind.
#[must_use]
pub fn meta(kind: spot_meta::MetaErrorKind) -> &'static str {
    use spot_meta::MetaErrorKind as K;
    match kind {
        K::Io => "nocode_model.meta_io",
        K::Parse => "nocode_model.meta_parse",
        K::FileName => "nocode_model.meta_file_name",
        K::InvalidName => "nocode_model.meta_invalid_name",
        K::ReservedName => "nocode_model.meta_reserved_name",
        K::Duplicate => "nocode_model.meta_duplicate",
        K::UnknownTarget => "nocode_model.meta_unknown_target",
        K::BadTable => "nocode_model.meta_bad_table",
        K::BadTitleField => "nocode_model.meta_bad_title_field",
        K::BadProperty => "nocode_model.meta_bad_property",
        K::BadDefault => "nocode_model.meta_bad_default",
        K::BadOptions => "nocode_model.meta_bad_options",
        K::BadIndex => "nocode_model.meta_bad_index",
        K::BadCustomField => "nocode_model.meta_bad_custom_field",
        K::BadOverride => "nocode_model.meta_bad_override",
        K::UnknownSeam => "nocode_model.meta_unknown_seam",
        K::NotYetSupported => "nocode_model.meta_not_yet_supported",
        K::UnknownName => "nocode_model.meta_unknown_name",
        K::Compile => "nocode_model.meta_compile",
        // The new declaration kinds share the nearest friendly code until they get their own.
        K::GeolocationNotAPoint | K::BadSearch | K::SearchFieldProtected | K::BadRange => {
            "nocode_model.meta_bad_property"
        }
        K::BadExclusion => "nocode_model.meta_bad_index",
        K::BackendUnsupported => "nocode_model.meta_not_yet_supported",
    }
}

/// The friendly code for a view validation error kind.
#[must_use]
pub fn view(kind: spot_views::ViewErrorKind) -> &'static str {
    use spot_views::ViewErrorKind as K;
    match kind {
        K::Parse => "nocode_model.view_parse",
        K::BadId => "nocode_model.view_bad_id",
        K::UnknownField => "nocode_model.view_unknown_field",
        K::UnknownControl => "nocode_model.view_unknown_control",
        K::UnknownSection => "nocode_model.view_unknown_section",
        K::UnknownTab => "nocode_model.view_unknown_tab",
        K::UnknownAnchor => "nocode_model.view_unknown_anchor",
        K::Duplicate => "nocode_model.view_duplicate",
        K::BadShape => "nocode_model.view_bad_shape",
        K::BadOp => "nocode_model.view_bad_op",
        K::HiddenRequired => "nocode_model.view_hidden_required",
    }
}

/// The stable `meta.<segment>` code of a metadata error kind, for the `source_code` a friendly
/// issue keeps. Built at run time so this crate never re-declares a code it does not own.
#[must_use]
pub fn meta_source(kind: spot_meta::MetaErrorKind) -> String {
    format!("meta.{}", meta_segment(kind))
}

/// The snake-case segment of a metadata error kind.
fn meta_segment(kind: spot_meta::MetaErrorKind) -> &'static str {
    use spot_meta::MetaErrorKind as K;
    match kind {
        K::Io => "io",
        K::Parse => "parse",
        K::FileName => "file_name",
        K::InvalidName => "invalid_name",
        K::ReservedName => "reserved_name",
        K::Duplicate => "duplicate",
        K::UnknownTarget => "unknown_target",
        K::BadTable => "bad_table",
        K::BadTitleField => "bad_title_field",
        K::BadProperty => "bad_property",
        K::BadDefault => "bad_default",
        K::BadOptions => "bad_options",
        K::BadIndex => "bad_index",
        K::BadCustomField => "bad_custom_field",
        K::BadOverride => "bad_override",
        K::UnknownSeam => "unknown_seam",
        K::NotYetSupported => "not_yet_supported",
        K::UnknownName => "unknown_name",
        K::Compile => "compile",
        K::GeolocationNotAPoint => "geolocation_not_a_point",
        K::BadExclusion => "bad_exclusion",
        K::BadSearch => "bad_search",
        K::SearchFieldProtected => "search_field_protected",
        K::BadRange => "bad_range",
        K::BackendUnsupported => "backend_unsupported",
    }
}
