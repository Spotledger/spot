//! The stable `migration_conflicts.*` codes, one per conflict category.
//!
//! Every conflict a [`ConflictItem`](crate::ConflictItem) carries names one of these constants,
//! and every constant has one entry in
//! `crates/framework/spot-errors/catalogue/migration_conflicts.toml`. The catalogue's summary is
//! the sentence a human reads; the code is the stable identity an admin UI, a report or a later
//! work package matches on.
//!
//! The codes are a library-internal vocabulary: nothing here raises an
//! [`spot_app_api::AppError`], so every catalogue entry's `class` is `none`.

/// A custom field's name collides with a field the release adds to the same DocType.
pub const CUSTOM_FIELD_COLLISION: &str = "migration_conflicts.custom_field_collision";
/// A custom field sits on a DocType the release removes.
pub const CUSTOM_FIELD_DOCTYPE_REMOVED: &str = "migration_conflicts.custom_field_doctype_removed";
/// A property override targets a field the release removes or renames.
pub const OVERRIDE_FIELD_REMOVED: &str = "migration_conflicts.override_field_removed";
/// A property override targets a DocType the release removes.
pub const OVERRIDE_DOCTYPE_REMOVED: &str = "migration_conflicts.override_doctype_removed";
/// A site `options` override no longer matches the new option list of the field.
pub const OVERRIDE_OPTIONS_CONFLICT: &str = "migration_conflicts.override_options_conflict";
/// A fixture document sets a field the release removes.
pub const FIXTURE_FIELD_REMOVED: &str = "migration_conflicts.fixture_field_removed";
/// A fixture targets a DocType the release removes.
pub const FIXTURE_DOCTYPE_REMOVED: &str = "migration_conflicts.fixture_doctype_removed";
/// A view layer references a field the release removes.
pub const VIEW_FIELD_REMOVED: &str = "migration_conflicts.view_field_removed";
/// A view layer targets a DocType the release removes.
pub const VIEW_DOCTYPE_REMOVED: &str = "migration_conflicts.view_doctype_removed";
/// A view layer hides a required field with no default, which strands a save.
pub const VIEW_HIDDEN_REQUIRED: &str = "migration_conflicts.view_hidden_required";
/// A script references a field that is not in the new declarations for its DocType.
pub const SCRIPT_TYPE_ERROR: &str = "migration_conflicts.script_type_error";
/// A script no longer transpiles (syntax or a script-tier rule).
pub const SCRIPT_SYNTAX_ERROR: &str = "migration_conflicts.script_syntax_error";
/// A script is bound to a DocType the release removes.
pub const SCRIPT_DOCTYPE_REMOVED: &str = "migration_conflicts.script_doctype_removed";
/// A rule or field expression no longer parses.
pub const RULE_PARSE_ERROR: &str = "migration_conflicts.rule_parse_error";
/// A rule or field expression references a field the release removes.
pub const RULE_FIELD_REMOVED: &str = "migration_conflicts.rule_field_removed";
/// An installed app targets a WIT world the new host does not serve.
pub const APP_WIT_WORLD_MISMATCH: &str = "migration_conflicts.app_wit_world_mismatch";
/// An installed app needs a DocType the release removes.
pub const APP_DOCTYPE_REMOVED: &str = "migration_conflicts.app_doctype_removed";
/// An installed app subscribes to a `field_changed` hook whose field the release removes.
pub const APP_HOOK_REMOVED: &str = "migration_conflicts.app_hook_removed";
/// An installed app's `requires_host` range excludes the new host version.
pub const APP_HOST_INCOMPATIBLE: &str = "migration_conflicts.app_host_incompatible";
/// The new metadata cannot generate the TypeScript declarations the script check needs.
pub const DECLARATIONS_ERROR: &str = "migration_conflicts.declarations_error";

/// Every code this crate can report, for a listing and the catalogue-coverage test.
pub const ALL: &[&str] = &[
    CUSTOM_FIELD_COLLISION,
    CUSTOM_FIELD_DOCTYPE_REMOVED,
    OVERRIDE_FIELD_REMOVED,
    OVERRIDE_DOCTYPE_REMOVED,
    OVERRIDE_OPTIONS_CONFLICT,
    FIXTURE_FIELD_REMOVED,
    FIXTURE_DOCTYPE_REMOVED,
    VIEW_FIELD_REMOVED,
    VIEW_DOCTYPE_REMOVED,
    VIEW_HIDDEN_REQUIRED,
    SCRIPT_TYPE_ERROR,
    SCRIPT_SYNTAX_ERROR,
    SCRIPT_DOCTYPE_REMOVED,
    RULE_PARSE_ERROR,
    RULE_FIELD_REMOVED,
    APP_WIT_WORLD_MISMATCH,
    APP_DOCTYPE_REMOVED,
    APP_HOOK_REMOVED,
    APP_HOST_INCOMPATIBLE,
    DECLARATIONS_ERROR,
];
