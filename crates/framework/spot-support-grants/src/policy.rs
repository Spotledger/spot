//! The deployment policy a grant must fit: the session limits plus the control plane's
//! own duration cap and condition budget.

use serde::{Deserialize, Serialize};

use spot_support_session::SessionPolicy;

/// The limits every grant is held to.
///
/// [`max_duration_secs`](Self::max_duration_secs) is the control plane's cap and may be
/// stricter than the session policy's own maximum; the effective cap is the smaller of
/// the two. There is deliberately no way to ask for a grant without an end: the record
/// carries a mandatory `expires_at` (a signed instant), so no "permanent" or
/// "until revoked" grant exists in the type system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPolicy {
    /// The session model's limits (approver distinctness, extensions, total duration).
    pub session: SessionPolicy,
    /// The control plane's own longest grant, measured from its start. Default: 4 hours,
    /// the same as the session default; a deployment may shorten it.
    pub max_duration_secs: i64,
    /// How many conditions a grant may carry. Default: 8.
    pub max_conditions: usize,
}

impl Default for GrantPolicy {
    /// The shipped policy: the session defaults, 4 hours and at most 8 conditions.
    fn default() -> GrantPolicy {
        GrantPolicy {
            session: SessionPolicy::default(),
            max_duration_secs: 4 * 3600,
            max_conditions: 8,
        }
    }
}

impl GrantPolicy {
    /// The effective duration cap: the smaller of the control plane's cap and the session
    /// policy's maximum.
    #[must_use]
    pub fn duration_cap(&self) -> i64 {
        self.max_duration_secs.min(self.session.max_duration_secs)
    }
}
