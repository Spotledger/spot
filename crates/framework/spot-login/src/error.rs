//! The `login.*` error vocabulary and its mapping onto the native API's `auth.*` codes.
//!
//! Every sign-in outcome has one stable `login.*` kind (the audit and `Login Attempt` result
//! code). The HTTP surface never sees a `login.*` code directly: the contract maps a failure to
//! the `auth.*` vocabulary, and — for a bad password, an unknown user, a disabled, invited or
//! anonymised account — every one of those becomes the single `auth.invalid_credentials`, so
//! the response never tells a caller which of them applied. The true reason is kept for the
//! audit record only.

use std::fmt;

use spot_http::api::{ApiError, Code, FieldIssue};
use spot_sessions::{SessionError, ThrottleError};

/// One stable `login.*` outcome kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoginErrorKind {
    /// Wrong password, unknown user, or a signable-state refusal the caller must not learn.
    InvalidCredentials,
    /// The identifier was empty or unusable after normalisation.
    InvalidIdentifier,
    /// The account exists but is disabled.
    AccountDisabled,
    /// The account exists but the invitation was never accepted.
    AccountInvited,
    /// The account was anonymised.
    AccountAnonymised,
    /// The account is hard-locked after repeated failures.
    AccountLocked,
    /// The attempt is backed off (exponential throttle).
    Throttled,
    /// The attempt was refused by the per-source throttle.
    ThrottleBurst,
    /// The user has a second factor and no code was supplied.
    TotpRequired,
    /// The supplied two-factor code was wrong.
    TotpInvalid,
    /// The supplied two-factor code was already used.
    TotpReplayed,
    /// The supplied recovery code was wrong.
    RecoveryCodeInvalid,
    /// The supplied recovery code was already used.
    RecoveryCodeUsed,
    /// `change_password`: the current password did not verify.
    CurrentPasswordWrong,
    /// `change_password`: the new password fails the tenant policy.
    PasswordPolicyViolation,
    /// No such session of the caller's.
    SessionNotFound,
    /// The target session belongs to another user.
    SessionForbidden,
    /// No API key has this id.
    ApiKeyNotFound,
    /// The API key was revoked.
    ApiKeyRevoked,
    /// The API key has expired.
    ApiKeyExpired,
    /// The API key's owner is not enabled.
    ApiKeyOwnerDisabled,
    /// An administrative action was refused.
    AdminActionDenied,
    /// A one-time initial secret was unknown or expired.
    InitialSecretInvalid,
    /// A one-time initial secret was already used.
    InitialSecretUsed,
    /// A store, event sink or clock failed.
    StoreFailed,
}

impl LoginErrorKind {
    /// The stable `login.*` code, as the error catalogue carries it.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            LoginErrorKind::InvalidCredentials => "login.invalid_credentials",
            LoginErrorKind::InvalidIdentifier => "login.invalid_identifier",
            LoginErrorKind::AccountDisabled => "login.account_disabled",
            LoginErrorKind::AccountInvited => "login.account_invited",
            LoginErrorKind::AccountAnonymised => "login.account_anonymised",
            LoginErrorKind::AccountLocked => "login.account_locked",
            LoginErrorKind::Throttled => "login.throttled",
            LoginErrorKind::ThrottleBurst => "login.throttle_burst",
            LoginErrorKind::TotpRequired => "login.totp_required",
            LoginErrorKind::TotpInvalid => "login.totp_invalid",
            LoginErrorKind::TotpReplayed => "login.totp_replayed",
            LoginErrorKind::RecoveryCodeInvalid => "login.recovery_code_invalid",
            LoginErrorKind::RecoveryCodeUsed => "login.recovery_code_used",
            LoginErrorKind::CurrentPasswordWrong => "login.current_password_wrong",
            LoginErrorKind::PasswordPolicyViolation => "login.password_policy_violation",
            LoginErrorKind::SessionNotFound => "login.session_not_found",
            LoginErrorKind::SessionForbidden => "login.session_forbidden",
            LoginErrorKind::ApiKeyNotFound => "login.api_key_not_found",
            LoginErrorKind::ApiKeyRevoked => "login.api_key_revoked",
            LoginErrorKind::ApiKeyExpired => "login.api_key_expired",
            LoginErrorKind::ApiKeyOwnerDisabled => "login.api_key_owner_disabled",
            LoginErrorKind::AdminActionDenied => "login.admin_action_denied",
            LoginErrorKind::InitialSecretInvalid => "login.initial_secret_invalid",
            LoginErrorKind::InitialSecretUsed => "login.initial_secret_used",
            LoginErrorKind::StoreFailed => "login.store_failed",
        }
    }

    /// A short, non-secret plain-language message for logs.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            LoginErrorKind::InvalidCredentials => "The credentials are wrong or no longer valid.",
            LoginErrorKind::InvalidIdentifier => "The sign-in name is empty or unusable.",
            LoginErrorKind::AccountDisabled => "The account is disabled.",
            LoginErrorKind::AccountInvited => "The invitation was never accepted.",
            LoginErrorKind::AccountAnonymised => "The account was anonymised.",
            LoginErrorKind::AccountLocked => "The account is locked after failed sign-ins.",
            LoginErrorKind::Throttled => "The attempt is backed off after failed sign-ins.",
            LoginErrorKind::ThrottleBurst => "The source address is sending too many attempts.",
            LoginErrorKind::TotpRequired => "A one-time code is required.",
            LoginErrorKind::TotpInvalid => "The one-time code is wrong.",
            LoginErrorKind::TotpReplayed => "The one-time code was already used.",
            LoginErrorKind::RecoveryCodeInvalid => "The recovery code is wrong.",
            LoginErrorKind::RecoveryCodeUsed => "The recovery code was already used.",
            LoginErrorKind::CurrentPasswordWrong => "The current password is wrong.",
            LoginErrorKind::PasswordPolicyViolation => "The new password fails the policy.",
            LoginErrorKind::SessionNotFound => "No such session of the caller's.",
            LoginErrorKind::SessionForbidden => "The session belongs to another user.",
            LoginErrorKind::ApiKeyNotFound => "No API key has this id.",
            LoginErrorKind::ApiKeyRevoked => "The API key was revoked.",
            LoginErrorKind::ApiKeyExpired => "The API key has expired.",
            LoginErrorKind::ApiKeyOwnerDisabled => "The API key's owner is not enabled.",
            LoginErrorKind::AdminActionDenied => "The administrative action was refused.",
            LoginErrorKind::InitialSecretInvalid => "The one-time secret is unknown or expired.",
            LoginErrorKind::InitialSecretUsed => "The one-time secret was already used.",
            LoginErrorKind::StoreFailed => "A backend dependency failed.",
        }
    }

    /// The HTTP status the contract registers for this kind's public code.
    #[must_use]
    pub fn api_code(self) -> Code {
        match self {
            // Everything that reveals nothing about why a sign-in failed shares one code.
            LoginErrorKind::InvalidCredentials
            | LoginErrorKind::InvalidIdentifier
            | LoginErrorKind::AccountDisabled
            | LoginErrorKind::AccountInvited
            | LoginErrorKind::AccountAnonymised
            | LoginErrorKind::ApiKeyNotFound
            | LoginErrorKind::ApiKeyRevoked
            | LoginErrorKind::ApiKeyExpired
            | LoginErrorKind::ApiKeyOwnerDisabled
            | LoginErrorKind::InitialSecretInvalid
            | LoginErrorKind::InitialSecretUsed => Code::AuthInvalidCredentials,
            LoginErrorKind::AccountLocked
            | LoginErrorKind::Throttled
            | LoginErrorKind::ThrottleBurst => Code::ApiRateLimited,
            LoginErrorKind::TotpRequired => Code::AuthTotpRequired,
            LoginErrorKind::TotpInvalid
            | LoginErrorKind::TotpReplayed
            | LoginErrorKind::RecoveryCodeInvalid
            | LoginErrorKind::RecoveryCodeUsed => Code::AuthTotpInvalid,
            LoginErrorKind::CurrentPasswordWrong => Code::AuthInvalidCredentials,
            LoginErrorKind::PasswordPolicyViolation => Code::AuthPasswordPolicy,
            // A session that is not the caller's reads exactly like one that does not exist.
            LoginErrorKind::SessionNotFound | LoginErrorKind::SessionForbidden => {
                Code::AuthSessionNotFound
            }
            LoginErrorKind::AdminActionDenied => Code::PermissionDenied,
            LoginErrorKind::StoreFailed => Code::HttpInternal,
        }
    }
}

/// A sign-in, session or key failure with its stable kind and a non-secret detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginError {
    /// The stable outcome kind.
    pub kind: LoginErrorKind,
    /// A non-secret detail string for logs; it never carries a credential or a raw address.
    pub detail: String,
    /// The typed retry marker: `Some(seconds)` on a [`LoginErrorKind::StoreFailed`] whose cause
    /// the store layer classified as storage being unavailable (decided from the storage
    /// error's kind, never from text), `None` on every other error. It is the one thing
    /// [`LoginError::is_unavailable`] reads.
    pub retryable_after: Option<u64>,
}

/// The seconds a client is told to wait before retrying an unavailable store.
const UNAVAILABLE_RETRY_SECS: u64 = 1;

/// A private prefix the stores put on the detail they hand to `spot-sessions`, whose error
/// types (`SessionError::StoreFailed(String)`, `ThrottleError::StoreFailed(String)`) carry text
/// only and are outside this crate. The stores add it from the typed storage error kind; the
/// `From` conversions below strip it and set [`LoginError::retryable_after`]. It starts with a
/// control character, which no store detail, identifier or storage message begins with, and it
/// is only ever read at the very start of the text.
const UNAVAILABLE_MARK: &str = "\u{1}unavailable\u{1}";

impl LoginError {
    /// Builds an error of `kind` with a non-secret detail.
    pub fn new(kind: LoginErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            retryable_after: None,
        }
    }

    /// A store failure whose cause was storage being unavailable (retryable): the typed marker
    /// is set, the detail stays the non-secret log text.
    pub fn store_unavailable(detail: impl Into<String>) -> Self {
        Self {
            kind: LoginErrorKind::StoreFailed,
            detail: detail.into(),
            retryable_after: Some(UNAVAILABLE_RETRY_SECS),
        }
    }

    /// Builds a store failure from the text a `spot-sessions` store error carries, restoring
    /// the typed marker the stores encoded with [`LoginError::store_text`].
    pub(crate) fn from_store_text(text: String) -> Self {
        match text.strip_prefix(UNAVAILABLE_MARK) {
            Some(rest) => Self::store_unavailable(rest),
            None => Self::new(LoginErrorKind::StoreFailed, text),
        }
    }

    /// The text to hand a `spot-sessions` store error: the detail, with the private marker in
    /// front when this error is a typed unavailable.
    pub(crate) fn store_text(&self) -> String {
        if self.is_unavailable() {
            format!("{UNAVAILABLE_MARK}{}", self.detail)
        } else {
            self.detail.clone()
        }
    }

    /// The stable `login.*` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// Renders the failure into the native API's error envelope vocabulary.
    ///
    /// A throttled or locked account carries `retry_after` as a detail; a password-policy
    /// violation is rendered through [`LoginError::password_policy`], which needs the violated
    /// rules.
    #[must_use]
    pub fn to_api_error(&self) -> ApiError {
        let code = self.kind.api_code();
        match self.kind {
            LoginErrorKind::Throttled | LoginErrorKind::ThrottleBurst => {
                ApiError::new(code, self.kind.message()).detail(
                    "retry_after",
                    serde_json::Value::from(self.retry_after_secs().unwrap_or(0)),
                )
            }
            LoginErrorKind::AccountLocked => ApiError::new(code, self.kind.message()).detail(
                "retry_after",
                serde_json::Value::from(self.retry_after_secs().unwrap_or(0)),
            ),
            LoginErrorKind::StoreFailed if self.is_unavailable() => ApiError::new(
                Code::ServerUnavailable,
                "The sign-in service is temporarily unavailable.",
            )
            .detail(
                "retry_after",
                serde_json::Value::from(
                    self.retryable_after
                        .unwrap_or(UNAVAILABLE_RETRY_SECS)
                        .max(1),
                ),
            ),
            LoginErrorKind::StoreFailed => {
                ApiError::new(code, "The sign-in service is temporarily unavailable.")
            }
            _ => ApiError::new(code, self.kind.message()),
        }
    }

    /// Whether this is a store failure whose cause was storage being unavailable (the pool
    /// could not give a connection within its wait, or the backend could not be reached). It
    /// reads the typed [`LoginError::retryable_after`] marker; no text is searched.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.kind == LoginErrorKind::StoreFailed && self.retryable_after.is_some()
    }

    /// Like [`LoginError::to_api_error`], and hands `log` one non-secret line (the stable code,
    /// the request id and the detail) exactly once when the failure is a store failure, so the
    /// caller can write it to its log. The line never reaches the client.
    #[must_use]
    pub fn to_api_error_logged(&self, request_id: &str, log: &mut dyn FnMut(&str)) -> ApiError {
        if self.kind == LoginErrorKind::StoreFailed {
            log(&format!(
                "{} request_id={} {}",
                self.code(),
                request_id,
                self.detail
            ));
        }
        self.to_api_error()
    }

    /// Like [`LoginError::to_api_error`], and writes one `tracing` event at `warn` level when the
    /// failure is a store failure, so an operator sees the retryable 503 that the client only
    /// sees as a friendly message. The event carries `request_id`, `kind` (the stable
    /// `login.*` code) and `storage_code`, and never the identifier, a credential, a secret or
    /// the detail text (which can hold a connection message).
    #[must_use]
    pub fn to_api_error_traced(&self, request_id: &str) -> ApiError {
        if self.kind == LoginErrorKind::StoreFailed {
            let storage_code = if self.is_unavailable() {
                spot_storage_contract::ErrorKind::Unavailable.code()
            } else {
                "unclassified"
            };
            tracing::warn!(
                request_id,
                kind = self.code(),
                storage_code,
                "sign-in store failure"
            );
        }
        self.to_api_error()
    }

    /// The wait in seconds a throttled or locked attempt must observe, parsed from the detail.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.detail
            .strip_prefix("retry_after=")
            .and_then(|text| text.parse::<u64>().ok())
    }

    /// Builds a policy-violation error carrying every violated rule as a field message.
    #[must_use]
    pub fn password_policy(violations: &[spot_auth::PolicyViolation]) -> ApiError {
        let mut error = ApiError::new(
            Code::AuthPasswordPolicy,
            LoginErrorKind::PasswordPolicyViolation.message(),
        );
        for violation in violations {
            error = error.with_field(FieldIssue::new(
                "new_password",
                violation.code,
                violation.message.clone(),
            ));
        }
        error
    }
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.detail)
    }
}

impl std::error::Error for LoginError {}

impl From<SessionError> for LoginError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::StoreFailed(detail) => LoginError::from_store_text(detail),
            SessionError::Invalid { .. } | SessionError::Malformed => {
                LoginError::new(LoginErrorKind::SessionNotFound, "the session is not valid")
            }
        }
    }
}

impl From<ThrottleError> for LoginError {
    fn from(error: ThrottleError) -> Self {
        match error {
            ThrottleError::StoreFailed(detail) => LoginError::from_store_text(detail),
            other @ ThrottleError::InvalidAccount(_) => {
                LoginError::new(LoginErrorKind::StoreFailed, other.to_string())
            }
        }
    }
}

/// The one external answer a bad password, an unknown user and a non-signable account share.
#[must_use]
pub fn invalid_credentials() -> ApiError {
    ApiError::new(
        Code::AuthInvalidCredentials,
        LoginErrorKind::InvalidCredentials.message(),
    )
}
