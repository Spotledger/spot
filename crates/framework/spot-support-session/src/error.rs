//! The one error type, with stable, machine-readable `session.*` codes (blueprint §4, in
//! the style of spot-permissions' `perm.*` codes).

/// A request, approval or extension failed validation, or an extension could not be
/// applied.
///
/// The codes are stable; messages are for people.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The request's reason is blank or whitespace only.
    #[error("the session grant needs a non-blank reason")]
    ReasonBlank,

    /// A scope names nothing: a `Named` or `Only` list that is empty after trimming.
    #[error("the scope names nothing: {0}")]
    ScopeEmpty(&'static str),

    /// The session would run longer than the policy's maximum duration.
    #[error("the session would run {asked} s, over the policy maximum of {max} s")]
    DurationOverPolicy {
        /// The asked duration, in seconds.
        asked: i64,
        /// The policy maximum, in seconds.
        max: i64,
    },

    /// The request's end is before its start, or its start is in the past.
    #[error("the session cannot end ({end}) before it starts ({start})")]
    EndBeforeStart {
        /// The start instant (unix seconds).
        start: i64,
        /// The end instant (unix seconds).
        end: i64,
    },

    /// The approval does not satisfy the policy: the approver equals the requester while
    /// the policy wants a distinct approver, or the ceiling is above
    /// [`crate::model::SessionPolicy::max_tier_without_owner_approval`] and the approver
    /// is not a tenant owner.
    #[error("the approval does not satisfy the policy: {0}")]
    ApprovalPolicy(String),

    /// The requested extension exceeds the policy: the extension count would pass the
    /// maximum, or the end would push the total duration past
    /// [`crate::model::SessionPolicy::max_duration_secs`] from the original start.
    #[error("the extension exceeds the policy: {0}")]
    ExtensionPolicy(String),

    /// The extension was asked on a grant that is no longer running: expired or revoked
    /// (neither can be undone; a new grant is the way back).
    #[error("the grant can no longer be extended: {0}")]
    NotExtendable(String),

    /// The extension's new end is at or before the current one, so it would change
    /// nothing.
    #[error("the extension's end ({end}) is at or before the current one ({current})")]
    ExtensionNotLater {
        /// The current end instant (unix seconds).
        current: i64,
        /// The proposed end instant (unix seconds).
        end: i64,
    },

    /// The lifecycle method was applied in a state that refuses it (for example
    /// approving twice).
    #[error("the grant is in the wrong state for that: {0}")]
    WrongState(String),
}
