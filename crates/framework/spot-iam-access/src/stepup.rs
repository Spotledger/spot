//! Step-up for sensitive operations and the sign-in method ladder (ADR-070 section 5).
//!
//! # The method ladder
//!
//! `password` < `password+totp` < `sso` = `hardware`. A session may stay at its method or move
//! to a stronger one; it can never move to a weaker one. `sso` and `hardware` rank the same
//! (neither is weaker than the other), but an SSO session cannot switch to a user that does
//! not allow SSO at all.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::{METHOD_SSO_NOT_ALLOWED, METHOD_WEAKER, STEP_UP_REQUIRED};

/// The name of an operation that may need a fresh, strong authentication.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationName(pub String);

impl OperationName {
    /// The name from text.
    #[must_use]
    pub fn new(name: &str) -> OperationName {
        OperationName(name.to_owned())
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The operations that always need step-up: role changes, API key creation, data export,
/// emergency access, user disable and impersonation approval. A tenant may add to the list,
/// never remove from it.
pub const DEFAULT_STEP_UP_OPERATIONS: [&str; 6] = [
    "role_change",
    "api_key_create",
    "data_export",
    "emergency_access",
    "user_disable",
    "impersonation_approval",
];

/// The longest a tenant may allow a sign-in to count as recent (seconds).
pub const MAX_STEP_UP_AGE_SECONDS: i64 = 3600;

/// The default age of a sign-in that still counts as recent (seconds).
pub const DEFAULT_STEP_UP_AGE_SECONDS: i64 = 600;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    operations: BTreeSet<OperationName>,
    #[serde(default = "default_age")]
    max_age_seconds: i64,
}

fn default_age() -> i64 {
    DEFAULT_STEP_UP_AGE_SECONDS
}

/// Which operations need step-up and how recent the strong sign-in must be.
///
/// The list always contains [`DEFAULT_STEP_UP_OPERATIONS`], whatever was written (also when it
/// is loaded from data), and the age is capped at [`MAX_STEP_UP_AGE_SECONDS`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawPolicy")]
pub struct StepUpPolicy {
    operations: BTreeSet<OperationName>,
    max_age_seconds: i64,
}

impl From<RawPolicy> for StepUpPolicy {
    fn from(raw: RawPolicy) -> StepUpPolicy {
        StepUpPolicy::new(raw.operations, raw.max_age_seconds)
    }
}

impl Default for StepUpPolicy {
    fn default() -> StepUpPolicy {
        StepUpPolicy::new([], DEFAULT_STEP_UP_AGE_SECONDS)
    }
}

impl StepUpPolicy {
    /// The default list plus `extra`, with `max_age_seconds` clamped to `0..=3600`.
    #[must_use]
    pub fn new(
        extra: impl IntoIterator<Item = OperationName>,
        max_age_seconds: i64,
    ) -> StepUpPolicy {
        let mut operations: BTreeSet<OperationName> = DEFAULT_STEP_UP_OPERATIONS
            .iter()
            .map(|o| OperationName::new(o))
            .collect();
        operations.extend(extra);
        StepUpPolicy {
            operations,
            max_age_seconds: max_age_seconds.clamp(0, MAX_STEP_UP_AGE_SECONDS),
        }
    }

    /// This policy with one more operation (the list only grows).
    #[must_use]
    pub fn with_operation(mut self, op: OperationName) -> StepUpPolicy {
        self.operations.insert(op);
        self
    }

    /// The operations that need step-up.
    #[must_use]
    pub fn operations(&self) -> &BTreeSet<OperationName> {
        &self.operations
    }

    /// How recent the strong sign-in must be, in seconds.
    #[must_use]
    pub fn max_age_seconds(&self) -> i64 {
        self.max_age_seconds
    }

    /// Whether `op` needs a fresh strong sign-in, given when the user last authenticated
    /// strongly. A sign-in dated in the future counts as stale.
    #[must_use]
    pub fn needs_step_up(&self, op: &OperationName, last_auth_at: i64, now: i64) -> bool {
        if !self.operations.contains(op) {
            return false;
        }
        last_auth_at > now || now.saturating_sub(last_auth_at) > self.max_age_seconds
    }

    /// [`needs_step_up`](StepUpPolicy::needs_step_up) as a result.
    ///
    /// # Errors
    ///
    /// [`StepUpRequired`] when the operation needs a fresh sign-in.
    pub fn require(
        &self,
        op: &OperationName,
        last_auth_at: i64,
        now: i64,
    ) -> Result<(), StepUpRequired> {
        if self.needs_step_up(op, last_auth_at, now) {
            Err(StepUpRequired {
                operation: op.clone(),
            })
        } else {
            Ok(())
        }
    }
}

/// The operation needs a recent strong sign-in.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("iamaccess.step_up_required: `{}` needs a recent strong sign-in", .operation.as_str())]
pub struct StepUpRequired {
    /// The operation.
    pub operation: OperationName,
}

impl StepUpRequired {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        STEP_UP_REQUIRED
    }
}

/// How a session was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// A password alone.
    Password,
    /// A password and a time-based one-time code.
    #[serde(rename = "password_totp")]
    PasswordTotp,
    /// Single sign-on.
    Sso,
    /// A hardware key.
    Hardware,
}

impl AuthMethod {
    /// The rung on the ladder: `password` 0, `password+totp` 1, `sso` and `hardware` 2.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            AuthMethod::Password => 0,
            AuthMethod::PasswordTotp => 1,
            AuthMethod::Sso | AuthMethod::Hardware => 2,
        }
    }

    /// Whether `self` is a strictly weaker method than `other`.
    #[must_use]
    pub fn is_weaker_than(self, other: AuthMethod) -> bool {
        self.rank() < other.rank()
    }
}

/// What the user a session would switch to allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct TargetSignIn {
    /// The strongest method the target user signs in with.
    pub method: AuthMethod,
    /// Whether the target user allows single sign-on at all.
    pub allows_sso: bool,
}

/// Why a session may not switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MethodError {
    /// An SSO session cannot switch to a user that does not allow SSO.
    #[error("iamaccess.method_sso_not_allowed: the user does not allow single sign-on")]
    SsoNotAllowed,
    /// The session cannot move to a weaker method.
    #[error("iamaccess.method_weaker: the session cannot move to a weaker sign-in method")]
    Weaker {
        /// The session's method.
        from: AuthMethod,
        /// The method it would move to.
        to: AuthMethod,
    },
}

impl MethodError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            MethodError::SsoNotAllowed => METHOD_SSO_NOT_ALLOWED,
            MethodError::Weaker { .. } => METHOD_WEAKER,
        }
    }
}

/// The switch rules.
#[derive(Clone, Copy, Debug, Default)]
pub struct MethodGuard;

impl MethodGuard {
    /// May a session established with `session_method` switch to the target user?
    ///
    /// # Errors
    ///
    /// [`MethodError::SsoNotAllowed`] for an SSO session and a target without SSO, and
    /// [`MethodError::Weaker`] when the target's method ranks below the session's.
    pub fn may_switch(
        session_method: AuthMethod,
        target: &TargetSignIn,
    ) -> Result<(), MethodError> {
        if session_method == AuthMethod::Sso && !target.allows_sso {
            return Err(MethodError::SsoNotAllowed);
        }
        if target.method.is_weaker_than(session_method) {
            return Err(MethodError::Weaker {
                from: session_method,
                to: target.method,
            });
        }
        Ok(())
    }
}
