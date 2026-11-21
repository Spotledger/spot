//! The `sync_policy.*` error vocabulary (WP-EDG-08.T1).
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/sync_policy.toml`.
//! Callers match on [`PolicyError::code`] or [`crate::PolicyProblem::code`], never on the
//! message. Messages name DocTypes, fields and scope key types; they never carry a secret.

use spot_app_api::{AppError, ErrorClass};

/// The stable dotted codes this crate raises or defines. Each is catalogued in
/// `sync_policy.toml`; the error-code completeness scan proves both directions.
pub mod codes {
    /// A property key is not one of the sync policy's keys.
    pub const UNKNOWN_PROPERTY: &str = "sync_policy.unknown_property";
    /// A property value is not a valid class, direction, trigger, scope key type or field name.
    pub const BAD_VALUE: &str = "sync_policy.bad_value";
    /// A declared policy names a DocType the metadata does not hold.
    pub const UNKNOWN_DOCTYPE: &str = "sync_policy.unknown_doctype";
    /// A synced DocType links, directly or through a child table, to a `local` DocType.
    pub const CLOSED_LINK: &str = "sync_policy.closed_link";
    /// A policy's direction contradicts its class.
    pub const DIRECTION_CONFLICT: &str = "sync_policy.direction_conflict";
    /// A policy that flows upward names no `owner_scope`, so ownership cannot be checked.
    pub const OWNER_SCOPE_MISSING: &str = "sync_policy.owner_scope_missing";
    /// An excluded field is required, is a link, or is not a field of the DocType.
    pub const EXCLUDED_FIELD: &str = "sync_policy.excluded_field";
    /// A policy change would strand records already queued for delivery.
    pub const ORPHAN_QUEUED: &str = "sync_policy.orphan_queued";
    /// A received document still carries a field the policy excludes.
    pub const EXCLUDED_FIELD_RECEIVED: &str = "sync_policy.excluded_field_received";
}

/// A refusal while reading the policy text form. Its `code` is one of the `sync_policy.*`
/// parse codes; the message names the key or value, never a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError {
    code: &'static str,
    detail: String,
}

impl PolicyError {
    /// An error with the given catalogued code and a plain-language detail.
    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// The key is not one of the sync policy's property keys.
    #[must_use]
    pub fn unknown_property(key: &str) -> Self {
        Self::new(
            codes::UNKNOWN_PROPERTY,
            format!("`{key}` is not a sync policy property"),
        )
    }

    /// The value is not valid for the key.
    #[must_use]
    pub fn bad_value(key: &str, reason: impl Into<String>) -> Self {
        Self::new(
            codes::BAD_VALUE,
            format!("`{key}` is invalid: {}", reason.into()),
        )
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The plain-language detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The full message: the code and the detail.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }

    /// The platform error, classed [`ErrorClass::Validation`].
    #[must_use]
    pub fn to_error(&self) -> AppError {
        AppError::new(ErrorClass::Validation, self.code, self.message())
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PolicyError {}
