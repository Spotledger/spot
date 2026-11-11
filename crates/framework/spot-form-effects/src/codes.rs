//! The crate's `form_effects.*` codes, one per entry in
//! `crates/framework/spot-errors/catalogue/form_effects.toml`.

/// A batch names a protocol version this server does not speak.
pub const PROTOCOL_VERSION: &str = "form_effects.protocol_version";
/// An effect's `kind` is not one this protocol version knows.
pub const EFFECT_UNKNOWN: &str = "form_effects.effect_unknown";
/// An effect does not have the object shape its kind requires.
pub const EFFECT_INVALID: &str = "form_effects.effect_invalid";
/// A field path is not `field` or `table[idx].field`.
pub const PATH_INVALID: &str = "form_effects.path_invalid";
/// The DocType has no such field, table, column or row.
pub const PATH_UNKNOWN: &str = "form_effects.path_unknown";
/// The field exists but the triggering user may not see it. The problem never names it.
pub const PATH_UNAVAILABLE: &str = "form_effects.path_unavailable";
/// The value does not fit the field's kind, scale, length or listed values.
pub const VALUE_INVALID: &str = "form_effects.value_invalid";
/// The triggering user may not write the field, or the field is read-only.
pub const FIELD_NOT_WRITABLE: &str = "form_effects.field_not_writable";
/// A select or link's candidate values are malformed, duplicated or too many.
pub const OPTION_INVALID: &str = "form_effects.option_invalid";
/// A label is empty or too long.
pub const LABEL_INVALID: &str = "form_effects.label_invalid";
/// A message has neither text nor a catalogue key, or its text is too long.
pub const MESSAGE_INVALID: &str = "form_effects.message_invalid";
/// An action's id or label is malformed.
pub const ACTION_INVALID: &str = "form_effects.action_invalid";
/// A dialog is not registered for this form.
pub const DIALOG_UNKNOWN: &str = "form_effects.dialog_unknown";
/// A filter expression does not parse, or the field it is set on is not a link.
pub const FILTER_INVALID: &str = "form_effects.filter_invalid";
/// A script produced more effects, or larger ones, than the caps allow.
pub const LIMIT_EXCEEDED: &str = "form_effects.limit_exceeded";
/// A script failed for a reason the user must not see; one safe message is shown.
pub const SCRIPT_FAILED: &str = "form_effects.script_failed";
/// A registered form script's record is missing or malformed.
pub const SCRIPT_RECORD_INVALID: &str = "form_effects.script_record_invalid";
/// A script's exported handler is missing.
pub const MISSING_HANDLER: &str = "form_effects.missing_handler";
/// A script's TypeScript source did not compile.
pub const COMPILE_FAILED: &str = "form_effects.compile_failed";
/// A script returned a document patch that does not fit the DocType.
pub const PATCH_INVALID: &str = "form_effects.patch_invalid";
/// A script's returned document patch names a field the user may not change, or one that does
/// not exist or is hidden. The whole patch is rejected and the problem names no field.
pub const FIELD_UNAVAILABLE: &str = "form_effects.field_unavailable";
/// A script logged more lines than the per-run cap; the surplus was dropped. Reported once.
pub const LOG_TRUNCATED: &str = "form_effects.log_truncated";
