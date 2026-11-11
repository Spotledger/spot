//! The no-weaker-method rule, delegating to `spot-iam-access` (ADR-070 section 5.3).
//!
//! This crate never re-implements the [`AuthMethod`] ladder: [`session_method_change`] is the
//! one place a sign-in asks whether a session may move, and the answer is the access crate's.

pub use spot_iam_access::{AuthMethod, MethodError, MethodGuard, TargetSignIn};

/// Whether a session at `current` may switch to `target`.
///
/// # Errors
///
/// The [`MethodError`] of `spot-iam-access`: an SSO session may not switch to a user that does
/// not allow SSO, and no session may move to a weaker method.
pub fn session_method_change(
    current: AuthMethod,
    target: &TargetSignIn,
) -> core::result::Result<(), MethodError> {
    MethodGuard::may_switch(current, target)
}
