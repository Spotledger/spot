//! The settings, the stored record and the answers of emergency access.
//!
//! A stored record is one JSON value: a version, the site and the grant id it belongs to, the
//! state machine of `spot-iam-questions` (`EmergencyGrant`, serialised through its serde types)
//! and what the host adds (the activation instant fixed before the assignment is written, the
//! assignment record, the recording status), with a SHA-256 digest of all of it. A record whose
//! version is unknown, whose site or id differs from its key, or whose digest does not match is
//! refused loudly ([`EmergencyError`] `state_invalid` or `state_version`) and never repaired. The
//! digest catches an accident, a truncation or an edit that forgot it; it is not a MAC (the host
//! holds no key for it), so the tamper evidence that matters is the access-change journal, which
//! is hash-chained.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_iam_governance::{EscalationTarget, OverrideSubmission};
use spot_iam_questions::{EmergencyGrant, EmergencyPhase, EmergencyPolicy, ReviewState};

/// The stored record's format version.
pub const STORED_VERSION: u32 = 1;

/// The service is not enabled on this site.
pub const CODE_DISABLED: &str = "emergency_access_host.disabled";
/// A setting is missing or out of range; the site does not start with it.
pub const CODE_SETTINGS_INVALID: &str = "emergency_access_host.settings_invalid";
/// A part (the store, the directory, the journal) cannot answer now; nothing was changed.
pub const CODE_UNAVAILABLE: &str = "emergency_access_host.unavailable";
/// No such grant, or one of another tenant: the same answer.
pub const CODE_NOT_FOUND: &str = "emergency_access_host.not_found";
/// A user may request emergency access only for themselves.
pub const CODE_NOT_SELF: &str = "emergency_access_host.not_self";
/// The named approver is not one the tenant's policy allows.
pub const CODE_APPROVER_NOT_ALLOWED: &str = "emergency_access_host.approver_not_allowed";
/// The approval process cannot be started now (no process host); the request waits.
pub const CODE_APPROVAL_UNAVAILABLE: &str = "emergency_access_host.approval_unavailable";
/// The segregation-of-duties gate refused the grant.
pub const CODE_CONFLICT: &str = "emergency_access_host.conflict";
/// The grant would conflict and the policy allows an override only with a complete submission.
pub const CODE_OVERRIDE_REQUIRED: &str = "emergency_access_host.override_required";
/// The stored record does not match its key or its digest.
pub const CODE_STATE_INVALID: &str = "emergency_access_host.state_invalid";
/// The stored record is of a format version this release does not read.
pub const CODE_STATE_VERSION: &str = "emergency_access_host.state_version";
/// The change could not be written to the access-change journal; nothing was changed.
pub const CODE_AUDIT_UNAVAILABLE: &str = "emergency_access_host.audit_unavailable";
/// No eligible controller exists for the review of an ended session.
pub const CODE_REVIEW_UNASSIGNED: &str = "emergency_access_host.review_unassigned";
/// The session recorded as many actions as one grant may hold.
pub const CODE_SESSION_FULL: &str = "emergency_access_host.session_full";
/// The recording of an ended session could not be archived; the status says so.
pub const CODE_RECORDING_UNAVAILABLE: &str = "emergency_access_host.recording_unavailable";
/// The caller may not do this to this grant.
pub const CODE_NOT_PERMITTED: &str = "emergency_access_host.not_permitted";
/// The assignment could not be written or ended; nothing was granted.
pub const CODE_WRITE_FAILED: &str = "emergency_access_host.write_failed";

/// The longest text a reason, a note or a detail may have, in bytes.
pub const MAX_TEXT: usize = 1024;
/// The most actions one session records.
pub const MAX_ACTIONS: usize = 2_000;

/// Why a call was refused or failed: a stable code, never a message with a value in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmergencyError {
    /// The catalogued code (`emergency_access_host.*` or the state machine's `iam_questions.*`
    /// and `iam_governance.*`).
    pub code: &'static str,
}

impl EmergencyError {
    /// An error with `code`.
    #[must_use]
    pub fn new(code: &'static str) -> Self {
        Self { code }
    }
}

impl std::fmt::Display for EmergencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for EmergencyError {}

/// `[services.emergency_access]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Off by default: a site opts in.
    pub enabled: bool,
    /// The longest session, in minutes (1 to 1440).
    pub max_duration_minutes: u64,
    /// The roles that may be requested this way. Must not be empty when enabled: an empty list
    /// would be "any role", and a firefighter path is never open-ended.
    pub emergency_roles: Vec<String>,
    /// Who may be named as the approver: user ids or `role:<name>`. Must not be empty.
    pub approvers: Vec<String>,
    /// The users who review an ended session (the first who is not the session's user is the
    /// controller). Must not be empty.
    pub controllers: Vec<String>,
    /// How long the approver has, in minutes.
    pub approval_sla_minutes: u64,
    /// How long the controller has to review, in hours (1 to 720).
    pub review_within_hours: u64,
    /// How long after it falls due an unconfirmed review waits before it escalates again.
    pub escalate_after_hours: u64,
    /// How many times a review escalates (1 to 10).
    pub max_escalations: u8,
    /// Where an unanswered approval or review escalates: a user id or `role:<name>`. Required.
    pub escalate_to: String,
    /// Whether a controller who is also the approver needs a second, distinct check.
    pub second_check: bool,
    /// Whether the approver may override a segregation-of-duties conflict (with a reason and a
    /// mitigating control, recorded). Off by default: a conflict refuses the grant.
    pub allow_sod_override: bool,
    /// How often the sweep runs, in seconds (1 to 60).
    pub sweep_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_duration_minutes: 60,
            emergency_roles: Vec::new(),
            approvers: Vec::new(),
            controllers: Vec::new(),
            approval_sla_minutes: 60,
            review_within_hours: 24,
            escalate_after_hours: 24,
            max_escalations: 3,
            escalate_to: String::new(),
            second_check: true,
            allow_sod_override: false,
            sweep_seconds: 5,
        }
    }
}

/// The settings, validated, as the service uses them.
#[derive(Clone, Debug)]
pub struct Policy {
    /// The state machine's policy.
    pub emergency: EmergencyPolicy,
    /// Who may be named as approver.
    pub approvers: Vec<String>,
    /// The review controllers.
    pub controllers: Vec<String>,
    /// The validated escalation target.
    pub escalate_to: String,
    /// Whether the approver may override a conflict.
    pub allow_sod_override: bool,
    /// The sweep interval, in seconds.
    pub sweep_seconds: u64,
}

fn names(list: &[String]) -> bool {
    !list.is_empty()
        && list
            .iter()
            .all(|n| !n.trim().is_empty() && n.len() <= 200 && !n.chars().any(char::is_control))
}

impl Settings {
    /// Checks every setting once, at start.
    ///
    /// # Errors
    ///
    /// The name of the setting that is wrong (never its value): the site does not start with it.
    pub fn validate(&self) -> Result<Policy, &'static str> {
        if !(1..=1440).contains(&self.max_duration_minutes) {
            return Err("max_duration_minutes");
        }
        if !names(&self.emergency_roles) {
            return Err("emergency_roles");
        }
        if !names(&self.approvers) {
            return Err("approvers");
        }
        if !names(&self.controllers) {
            return Err("controllers");
        }
        if !(1..=10_080).contains(&self.approval_sla_minutes) {
            return Err("approval_sla_minutes");
        }
        if !(1..=720).contains(&self.review_within_hours) {
            return Err("review_within_hours");
        }
        if !(1..=720).contains(&self.escalate_after_hours) {
            return Err("escalate_after_hours");
        }
        if !(1..=10).contains(&self.max_escalations) {
            return Err("max_escalations");
        }
        if !(1..=60).contains(&self.sweep_seconds) {
            return Err("sweep_seconds");
        }
        // Recorded security finding: an empty, blank or malformed escalation target refuses the start.
        let target = EscalationTarget::new(&self.escalate_to).map_err(|_| "escalate_to")?;
        Ok(Policy {
            emergency: EmergencyPolicy {
                max_duration_secs: self.max_duration_minutes * 60,
                approval_sla_secs: self.approval_sla_minutes * 60,
                review_due_secs: self.review_within_hours * 3600,
                escalate_after_secs: self.escalate_after_hours * 3600,
                max_escalations: self.max_escalations,
                require_second_check_when_approver_reviews: self.second_check,
                emergency_roles: self.emergency_roles.clone(),
            },
            approvers: self.approvers.clone(),
            controllers: self.controllers.clone(),
            escalate_to: target.as_str().to_owned(),
            allow_sod_override: self.allow_sod_override,
            sweep_seconds: self.sweep_seconds,
        })
    }
}

/// Where the recording of a session is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    /// The session has not ended; its actions are in the record.
    Open,
    /// Archived (sealed, append-only) with this head digest.
    Archived {
        /// The head digest, hex.
        digest: String,
    },
    /// The archive could not be written (no archive wired, or it refused): the actions are in
    /// the record and the audit trail only. Said, never skipped.
    Unavailable,
}

impl RecordingStatus {
    /// A stable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Archived { .. } => "archived",
            Self::Unavailable => "unavailable",
        }
    }
}

/// What a stored record holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    /// The format version.
    pub v: u32,
    /// The site the record belongs to.
    pub site: String,
    /// The grant id: the key the record is stored under.
    pub id: String,
    /// The state machine.
    pub grant: EmergencyGrant,
    /// The instant activation was fixed at, set before the assignment is written so a retry
    /// after a crash writes the same window.
    #[serde(default)]
    pub activation_at: Option<i64>,
    /// The assignment record the activation wrote.
    #[serde(default)]
    pub assignment: Option<String>,
    /// The recording.
    pub recording: RecordingStatus,
    /// The override the approver submitted for a segregation-of-duties conflict (a reason and a
    /// mitigating control), kept so the activation, which may run on another node, can use it.
    #[serde(default)]
    pub override_submission: Option<OverrideSubmission>,
    /// Why the activation or the review cannot proceed now (a stable code): it is retried
    /// quietly, and the status says so.
    #[serde(default)]
    pub blocked: Option<String>,
}

/// A stored record: the body and its digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    /// The content.
    pub body: Body,
    /// SHA-256 of the body's JSON, hex.
    pub digest: String,
}

fn digest_of(body: &Body) -> Option<String> {
    let json = serde_json::to_string(body).ok()?;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(json.as_bytes()) {
        out.push_str(&format!("{byte:02x}"));
    }
    Some(out)
}

impl Stored {
    /// A new record for `grant`.
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(site: &str, grant: EmergencyGrant) -> Body {
        Body {
            v: STORED_VERSION,
            site: site.to_owned(),
            id: grant.id().to_owned(),
            grant,
            activation_at: None,
            assignment: None,
            recording: RecordingStatus::Open,
            override_submission: None,
            blocked: None,
        }
    }

    /// The text to store for `body`.
    ///
    /// # Errors
    ///
    /// `state_invalid` when the body cannot be encoded.
    pub fn encode(body: &Body) -> Result<String, EmergencyError> {
        let digest = digest_of(body).ok_or(EmergencyError::new(CODE_STATE_INVALID))?;
        serde_json::to_string(&Stored {
            body: body.clone(),
            digest,
        })
        .map_err(|_| EmergencyError::new(CODE_STATE_INVALID))
    }

    /// Reads a stored text for `site` under key `id`.
    ///
    /// # Errors
    ///
    /// `state_version` for a version this release does not read; `state_invalid` for anything
    /// that does not parse, does not match its key or site, or does not match its digest.
    pub fn decode(text: &str, site: &str, id: &str) -> Result<Body, EmergencyError> {
        let raw: serde_json::Value =
            serde_json::from_str(text).map_err(|_| EmergencyError::new(CODE_STATE_INVALID))?;
        let version = raw
            .get("body")
            .and_then(|b| b.get("v"))
            .and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(STORED_VERSION)) {
            return Err(EmergencyError::new(if version.is_some() {
                CODE_STATE_VERSION
            } else {
                CODE_STATE_INVALID
            }));
        }
        let stored: Stored =
            serde_json::from_value(raw).map_err(|_| EmergencyError::new(CODE_STATE_INVALID))?;
        if stored.body.site != site
            || stored.body.id != id
            || stored.body.grant.id() != id
            || digest_of(&stored.body).as_deref() != Some(stored.digest.as_str())
        {
            return Err(EmergencyError::new(CODE_STATE_INVALID));
        }
        Ok(stored.body)
    }
}

/// What a caller sees of a review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewView {
    /// The controller.
    pub controller: String,
    /// `pending`, `confirmed`, `flagged` or `escalated`.
    pub state: &'static str,
    /// When it falls due.
    pub due_at: i64,
    /// Checks recorded.
    pub checks: usize,
    /// Checks needed.
    pub checks_required: u8,
    /// Escalations so far.
    pub escalations: u8,
}

/// What a caller sees of one grant: no secret, no before or after snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GrantView {
    /// The grant id.
    pub id: String,
    /// The user the session is for.
    pub user: String,
    /// The elevated role.
    pub role: String,
    /// The approver named.
    pub approver: String,
    /// The reason given.
    pub reason: String,
    /// The phase label.
    pub phase: &'static str,
    /// When the session ends, once active.
    pub ends_at: Option<i64>,
    /// The assignment record, once granted.
    pub assignment: Option<String>,
    /// The review, once the session ended.
    pub review: Option<ReviewView>,
    /// `open`, `archived` or `unavailable`.
    pub recording: &'static str,
    /// Actions recorded.
    pub actions: usize,
    /// The code of why the grant cannot proceed now, when it cannot.
    pub blocked: Option<String>,
}

impl GrantView {
    /// The view of `body`.
    #[must_use]
    pub fn of(body: &Body) -> GrantView {
        let grant = &body.grant;
        let ends_at = match &grant.phase {
            EmergencyPhase::Active { ends_at, .. } => Some(*ends_at),
            _ => None,
        };
        GrantView {
            id: body.id.clone(),
            user: grant.request.user.clone(),
            role: grant.request.role.as_str().to_owned(),
            approver: grant.request.approver.clone(),
            reason: grant.request.reason.clone(),
            phase: grant.phase.label(),
            ends_at,
            assignment: body.assignment.clone(),
            review: grant.review.as_ref().map(|r| ReviewView {
                controller: r.controller.clone(),
                state: match r.state {
                    ReviewState::Pending => "pending",
                    ReviewState::Confirmed { .. } => "confirmed",
                    ReviewState::Flagged { .. } => "flagged",
                    ReviewState::Escalated { .. } => "escalated",
                },
                due_at: r.due_at,
                checks: r.checks.len(),
                checks_required: r.checks_required,
                escalations: r.escalations,
            }),
            recording: body.recording.label(),
            actions: grant.actions.len(),
            blocked: body.blocked.clone(),
        }
    }
}

/// What a user asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRequest {
    /// The elevated role.
    pub role: String,
    /// Why; required.
    pub reason: String,
    /// The approver named (an id or `role:<name>` the policy allows).
    pub approver: String,
    /// The requested length, in minutes.
    pub minutes: u64,
}
