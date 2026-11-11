//! The stable error codes this crate raises, each with one entry in
//! `spot-errors/catalogue/bpm_entry.toml`.
//!
//! They are `pub const`s so the catalogue scan finds them and a caller can match on them without
//! a string literal.

/// A draft record is owned by a running request and was changed from another channel.
pub const OWNED_BY_REQUEST: &str = "bpm_entry.owned_by_request";

/// A `process_only` record type was created outside its entry process; the refusal names the
/// process to start.
pub const START_PROCESS: &str = "bpm_entry.start_process";

/// A form step's configuration, or the entry setting, did not parse or is incomplete.
pub const CONFIG_INVALID: &str = "bpm_entry.config_invalid";

/// The entered values failed the record type's validation; the step stays open.
pub const FORM_INVALID: &str = "bpm_entry.form_invalid";

/// The draft record could not be created, updated, released or discarded through its lifecycle.
pub const DRAFT_FAILED: &str = "bpm_entry.draft_failed";

/// The form step's task is not open, or the person may not act on it.
pub const STEP_NOT_OPEN: &str = "bpm_entry.step_not_open";

/// The form step resolved to nobody to ask.
pub const NO_ASSIGNEE: &str = "bpm_entry.no_assignee";

/// The task inbox could not be reached.
pub const INBOX: &str = "bpm_entry.inbox";

/// The step transition and the draft write could not be committed together; nothing was stored.
pub const COMMIT_FAILED: &str = "bpm_entry.commit_failed";

/// A batch run is in a state that does not allow the call (already completed, or it must be
/// resumed).
pub const BATCH_STATE: &str = "bpm_entry.batch_state";

/// A batch run reached its configured failure limit and stopped.
pub const BATCH_LIMIT: &str = "bpm_entry.batch_limit";

/// A batch run could not be started, stored or read.
pub const BATCH_FAILED: &str = "bpm_entry.batch_failed";

/// An entry setting or mode could not be read.
pub const SETTINGS: &str = "bpm_entry.settings";

/// The audit entry could not be written, so the operation failed closed.
pub const AUDIT: &str = "bpm_entry.audit";

/// A run label override was blank.
pub const LABEL_INVALID: &str = "bpm_entry.label_invalid";

/// A create named a company or unit that is unknown, or one the acting person may not use.
pub const SCOPE_INVALID: &str = "bpm_entry.scope_invalid";
