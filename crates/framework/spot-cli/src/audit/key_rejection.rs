//! The audit record of a refused API key: guessing keys leaves a trail like guessing passwords.
//!
//! The record names the site, the time, the *public* key id (only when the presented text parsed
//! into one), a reason class and the class of route. It never carries the secret part, the full
//! credential, the URL or a header. It is written through [`Recorder::failure`], so it shares the
//! sign-in failure budget and the `failures_suppressed` summary: anyone on the network can cause
//! these records. Nothing here counts toward a lockout.
use spot_auth::ApiKey;
use spot_login::{KeyRecord, KeyState};

use super::{AuditRecord, Recorder};

/// The longest API key id an audit record keeps, in characters.
pub const KEY_ID_MAX: usize = 16;

/// Why an API key was refused, as the audit may say it. A wrong secret for a known id is
/// [`Unknown`](RejectionReason::Unknown), so the trail cannot be used as an oracle for which ids
/// exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionReason {
    /// No such key, a wrong secret, or an owner that cannot sign in.
    Unknown,
    /// The secret was right but the key was revoked.
    Revoked,
    /// The secret was right but the key had expired.
    Expired,
    /// The text is not shaped like an API key.
    Malformed,
}

impl RejectionReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::Malformed => "malformed",
        }
    }
}

/// The class of route a credential was presented to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathClass {
    /// Any other route (the API and the files and print routes).
    Api,
    /// The metrics route.
    Metrics,
    /// The realtime socket.
    Realtime,
}

impl PathClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Metrics => "metrics",
            Self::Realtime => "realtime",
        }
    }

    /// The class of an HTTP path.
    #[must_use]
    pub fn of_path(path: &str) -> Self {
        match path {
            "/metrics" => Self::Metrics,
            "/api/v1/realtime" => Self::Realtime,
            _ => Self::Api,
        }
    }
}

/// The reason a refused credential is audited under. `found` is the stored key for the
/// presented id, when there is one. Only a presented secret that *matches* can be `revoked` or
/// `expired`: anything else reads `unknown`, whatever the id.
#[must_use]
pub fn classify(
    presented: &str,
    found: Option<&KeyRecord>,
    now: time::OffsetDateTime,
) -> RejectionReason {
    let Ok(key) = ApiKey::parse(presented) else {
        return RejectionReason::Malformed;
    };
    let Some(record) = found else {
        return RejectionReason::Unknown;
    };
    if record.key_id != key.key_id() || !record.digest.matches(&key.digest()) {
        return RejectionReason::Unknown;
    }
    if record.state != KeyState::Active {
        return RejectionReason::Revoked;
    }
    if record.expires_at.is_some_and(|at| now >= at) {
        return RejectionReason::Expired;
    }
    RejectionReason::Unknown
}

/// Audits a refused API key. Never errors and never changes the answer the caller gets: a record
/// over the budget is dropped and counted, a failed write is logged (kind only).
pub fn rejected(recorder: &Recorder, presented: &str, reason: RejectionReason, path: PathClass) {
    let mut record: AuditRecord = recorder.record("api_key_rejected");
    // The id of a text that parsed only: an unparseable credential records no id.
    record.key_id = ApiKey::parse(presented)
        .ok()
        .map(|key| key.key_id().chars().take(KEY_ID_MAX).collect());
    record.reason = Some(reason.as_str().to_owned());
    record.path = Some(path.as_str().to_owned());
    recorder.failure(&record);
}
