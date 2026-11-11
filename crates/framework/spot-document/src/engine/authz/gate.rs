//! The errors a permission refusal returns: `permission.denied` for an operation, with the
//! decision's **reason codes** in the support detail (never the rule text in the user-facing
//! message), and `permission.field_denied` for the fields a write may not touch.

use spot_app_api::{AppError, Document, ErrorClass, FieldPath};
use spot_perm_query::PermQueryError;
use spot_permissions::{Action, Decision, FIELD_DENIED, FieldViolation, Violation};

/// The refusal of an operation: `permission.denied`, the reason codes in the `reason` detail.
#[must_use]
pub fn denied(action: Action, doc: &Document, decision: &Decision) -> AppError {
    AppError::new(
        ErrorClass::PermissionDenied,
        "permission.denied",
        "The user may not perform this action.",
    )
    .detail("action", action.as_str())
    .detail("doctype", doc.doctype().as_str())
    .detail("reason", decision.codes().join(", "))
}

/// The refusal of a method call whose declared right is an app permission the principal does
/// not hold: `permission.denied`, the `action` detail the permission's name.
#[must_use]
pub fn permission_missing(permission: &str, doc: &Document) -> AppError {
    AppError::new(
        ErrorClass::PermissionDenied,
        "permission.denied",
        "The user may not perform this action.",
    )
    .detail("action", permission)
    .detail("doctype", doc.doctype().as_str())
    .detail("reason", "app_permission_missing")
}

/// The refusal of a method call whose declared right cannot be evaluated (a right this engine
/// does not know, or one that contradicts `mutates`): never a fallback to a weaker right.
#[must_use]
pub fn right_unknown(right: &str, doc: &Document) -> AppError {
    AppError::new(
        ErrorClass::PermissionDenied,
        "permission.denied",
        "The user may not perform this action.",
    )
    .detail("action", right)
    .detail("doctype", doc.doctype().as_str())
    .detail("reason", "method_right_unevaluable")
}

/// The refusal of a field write: `permission.field_denied`, one field error per path
/// (`table[idx].field` for a child row).
#[must_use]
pub fn field_denied(violations: &[FieldViolation]) -> AppError {
    let mut error = AppError::new(
        ErrorClass::PermissionDenied,
        "permission.field_denied",
        "The user may not set one or more of these fields.",
    );
    for violation in violations {
        error = error.with_field(path(&violation.path), FIELD_DENIED, denied_field_message());
    }
    error.detail("reason", FIELD_DENIED)
}

/// The refusal of a link value outside the acting user's scope: the same
/// `permission.field_denied` a field write gets, one field error per path.
#[must_use]
pub fn link_denied(violations: &[Violation]) -> AppError {
    let mut error = AppError::new(
        ErrorClass::PermissionDenied,
        "permission.field_denied",
        "The user may not link to that record.",
    );
    for violation in violations {
        error = error.with_field(path(&violation.path), FIELD_DENIED, link_message());
    }
    error.detail("reason", "user_permission_blocked")
}

/// The refusal of a list query the principal may not run.
#[must_use]
pub fn query_denied(error: &PermQueryError) -> AppError {
    if let PermQueryError::FieldNotFilterable { path: field } = error {
        return AppError::new(
            ErrorClass::PermissionDenied,
            "permission.field_denied",
            "The user may not filter, sort, group or aggregate on that field.",
        )
        .with_field(path(field), FIELD_DENIED, denied_field_message())
        .detail("reason", FIELD_DENIED);
    }
    AppError::new(
        ErrorClass::PermissionDenied,
        "permission.denied",
        "The query cannot be answered under the user's permissions.",
    )
    .detail("reason", error.code())
}

fn denied_field_message() -> &'static str {
    "The user may not set this field."
}

fn link_message() -> &'static str {
    "The value is outside the records this user may use."
}

fn path(field: &str) -> FieldPath {
    FieldPath::parse(field).unwrap_or_else(|| FieldPath::field(field))
}
