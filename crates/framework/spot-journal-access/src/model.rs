//! The grant, its audit events and the settings, with the constants of ADR-061 s7.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::error::AccessError;
use crate::timefmt::rfc3339;

/// The platform role name. It holds exactly one permission, [`PERMISSION`], and no document right.
pub const ROLE: &str = "journal_reader";
/// The one permission of the role.
pub const PERMISSION: &str = "journal.read";
/// The topic of a grant's lifecycle (`contracts/events`).
pub const GRANT_TOPIC: &str = "sys_journal_grant";
/// The topic of one access (`contracts/events`).
pub const ACCESS_TOPIC: &str = "sys_journal_access";
/// The literal `sys_journal_access.grant` holds for an administrator's read.
pub const ADMIN_GRANT: &str = "admin";
/// The actor of an expiry (no principal acts).
#[allow(dead_code)]
pub const SYSTEM_ACTOR: &str = "system";

/// Milliseconds in a day.
pub const DAY_MILLIS: i64 = 86_400_000;
/// The default maximum lifetime of a grant, in days.
pub const DEFAULT_MAX_GRANT_DAYS: u32 = 30;
/// The hard cap on `max_grant_days`.
pub const HARD_CAP_DAYS: u32 = 365;
/// The longest an external principal's grant may be at creation, in days.
pub const EXTERNAL_MAX_DAYS: u32 = 7;
/// No decision is cached longer than this.
pub const CACHE_MILLIS: i64 = 5_000;
/// The largest segment served, in bytes (the segment writer's own bound).
pub const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// The size of one streamed chunk.
pub const CHUNK_BYTES: usize = 256 * 1024;
/// The longest id (grantee, grantor, grant) accepted.
pub const MAX_ID_BYTES: usize = 128;

/// A boxed future, so the traits stay object safe on any executor.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, AccessError>> + Send + 'a>>;

/// The `[services.journal_access]` settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The maximum lifetime of any grant, in days (hard cap 365).
    pub max_grant_days: u32,
    /// Simultaneous segment streams per principal.
    pub max_concurrent_streams: u32,
    /// Bytes one principal may read per UTC day.
    pub daily_byte_budget: u64,
    /// The durability policy name `journal_info_get` reports (a name only).
    pub policy: String,
    /// The journal's region name `journal_info_get` reports.
    pub region: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_grant_days: DEFAULT_MAX_GRANT_DAYS,
            max_concurrent_streams: 4,
            daily_byte_budget: 64 * 1024 * 1024 * 1024,
            policy: "standard".to_owned(),
            region: String::new(),
        }
    }
}

impl Settings {
    /// Checks the bounds.
    ///
    /// # Errors
    /// [`AccessError::SettingsInvalid`] for a zero or over-cap lifetime, a zero concurrency or
    /// budget, or an over-long name.
    pub fn validate(&self) -> Result<(), AccessError> {
        if self.max_grant_days == 0 || self.max_grant_days > HARD_CAP_DAYS {
            return Err(AccessError::SettingsInvalid(
                "max_grant_days is from 1 to 365",
            ));
        }
        if self.max_concurrent_streams == 0 {
            return Err(AccessError::SettingsInvalid(
                "max_concurrent_streams is at least 1",
            ));
        }
        if self.daily_byte_budget == 0 {
            return Err(AccessError::SettingsInvalid(
                "daily_byte_budget is at least 1",
            ));
        }
        if self.policy.len() > 100 || self.region.len() > 100 {
            return Err(AccessError::SettingsInvalid(
                "policy and region are at most 100 bytes",
            ));
        }
        Ok(())
    }
}

/// One `journal_reader` grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The grant id (a record id).
    pub id: String,
    /// The principal who may read.
    pub grantee: String,
    /// The administrator who granted it.
    pub grantor: String,
    /// When it ends, Unix milliseconds.
    pub expires_at_millis: i64,
    /// Whether the grantee is an external principal (an auditor, a lakehouse job).
    pub external: bool,
    /// When it was made, Unix milliseconds.
    pub created_at_millis: i64,
    /// When it was revoked, if it was.
    pub revoked_at_millis: Option<i64>,
    /// Who revoked it.
    pub revoked_by: Option<String>,
}

/// What a grant is at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
    /// In force.
    Live,
    /// Its time ran out.
    Expired,
    /// An administrator revoked it.
    Revoked,
}

impl Grant {
    /// The state at `now_millis`. A revoked grant is revoked whatever its expiry.
    #[must_use]
    pub fn state(&self, now_millis: i64) -> GrantState {
        if self.revoked_at_millis.is_some() {
            GrantState::Revoked
        } else if self.expires_at_millis <= now_millis {
            GrantState::Expired
        } else {
            GrantState::Live
        }
    }
}

/// What happened to a grant (the `action` of `sys_journal_grant`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantAction {
    /// A new grant.
    Granted,
    /// A new expiry.
    Renewed,
    /// An administrator ended it.
    Revoked,
    /// A grant, renewal or revocation was refused.
    Refused,
}

impl GrantAction {
    /// The registered spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Renewed => "renewed",
            Self::Revoked => "revoked",
            Self::Refused => "refused",
        }
    }
}

/// One `sys_journal_grant` audit record, written in the same transaction as the grant row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantEvent {
    /// What happened.
    pub action: GrantAction,
    /// Who the grant is for.
    pub grantee: String,
    /// Who acted.
    pub grantor: String,
    /// The grant id (a fresh id for a refusal, which has no row).
    pub grant: String,
    /// The expiry, for granted and renewed.
    pub expires_at_millis: Option<i64>,
    /// Whether the grantee is external.
    pub external: bool,
    /// A bounded code for a refusal, never free text.
    pub reason: Option<String>,
}

impl GrantEvent {
    /// The payload of the registered `sys_journal_grant` topic.
    #[must_use]
    pub fn to_payload(&self) -> Json {
        let mut payload = json!({
            "action": self.action.as_str(),
            "grantee": self.grantee,
            "grantor": self.grantor,
            "grant": self.grant,
            "external": self.external,
        });
        if let (Some(map), Some(at)) = (payload.as_object_mut(), self.expires_at_millis) {
            map.insert("expires_at".to_owned(), Json::String(rfc3339(at)));
        }
        if let (Some(map), Some(reason)) = (payload.as_object_mut(), &self.reason) {
            map.insert("reason".to_owned(), Json::String(reason.clone()));
        }
        payload
    }
}

/// The object an access names (`sys_journal_access.object`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessObject {
    /// The generations list or a generation's manifest.
    Manifest,
    /// One sealed segment.
    Segment,
}

impl AccessObject {
    /// The registered spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Segment => "segment",
        }
    }
}

/// The outcome of an access (`sys_journal_access.outcome`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessOutcome {
    /// Bytes were served.
    Served,
    /// No usable grant, or a limit.
    Denied,
    /// The name is not in the signed manifest.
    NotFound,
    /// The grant ran out.
    Expired,
    /// The read failed after the grant passed.
    Failed,
}

impl AccessOutcome {
    /// The registered spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Served => "served",
            Self::Denied => "denied",
            Self::NotFound => "not_found",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }
}

/// One `sys_journal_access` audit record. It names the principal, the grant, the object and the
/// range, never content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessRecord {
    /// The caller.
    pub principal: String,
    /// The grant id, or [`ADMIN_GRANT`].
    pub grant: String,
    /// What was named.
    pub object: AccessObject,
    /// The generation (0 when the request named no valid one).
    pub generation: u64,
    /// The first sequence number of a segment request.
    pub first_seq: Option<u64>,
    /// The last sequence number of a segment request.
    pub last_seq: Option<u64>,
    /// What happened.
    pub outcome: AccessOutcome,
    /// A bounded code, never free text.
    pub reason: Option<String>,
    /// Bytes sent, for a served segment or manifest.
    pub bytes: Option<u64>,
}

impl AccessRecord {
    /// The payload of the registered `sys_journal_access` topic.
    #[must_use]
    pub fn to_payload(&self) -> Json {
        let mut payload = json!({
            "principal": self.principal,
            "grant": self.grant,
            "object": self.object.as_str(),
            "generation": self.generation,
            "outcome": self.outcome.as_str(),
        });
        if let Some(map) = payload.as_object_mut() {
            if let Some(first) = self.first_seq {
                map.insert("first_seq".to_owned(), json!(first));
            }
            if let Some(last) = self.last_seq {
                map.insert("last_seq".to_owned(), json!(last));
            }
            if let Some(reason) = &self.reason {
                map.insert("reason".to_owned(), Json::String(reason.clone()));
            }
            if let Some(bytes) = self.bytes {
                map.insert("bytes".to_owned(), json!(bytes));
            }
        }
        payload
    }
}

/// Whether `id` is acceptable as a principal or grant id here: one to [`MAX_ID_BYTES`] bytes,
/// no control character. Ids are always bind parameters; this only bounds them.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}
