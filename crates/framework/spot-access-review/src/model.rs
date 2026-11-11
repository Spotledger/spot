//! The data the state machine works on: the campaign spec, the snapshot items, the
//! deadline policy and the decisions. All serde and cloneable, so the identity service can
//! persist a mid-flight campaign and load it back identical.

/// One role assignment under review, by reference. The campaign never resolves it; the
/// identity service receives it back in [`crate::ReviewAction::RemoveAssignment`].
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct AssignmentRef(String);

impl AssignmentRef {
    /// Wraps the reference text. Open ([`crate::Campaign::open`]) is the validation gate:
    /// a blank reference fails there with `review.item_invalid`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The reference text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for AssignmentRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The recorded SoD override on an item (ADR-070 section 8 rule 1): why the conflicting
/// grant stands and what mitigating control covers it, as recorded at grant time. The
/// snapshot is taken by the caller; the campaign stores it unchanged.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct OverrideNote(String);

impl OverrideNote {
    /// Wraps the recorded override note. Validated (non-blank) where the campaign uses it,
    /// not here: [`crate::Campaign::open`] is the validation gate.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The recorded text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for OverrideNote {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a tenant does with items still undecided at the campaign's deadline
/// (ADR-070 section 8 rule 3: "unreviewed items past the deadline are escalated or, if the
/// tenant chooses, removed").
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeadlinePolicy {
    /// Escalate each undecided item to the named person, once; they may then decide.
    /// This is also the policy an item whose reviewer equals its user is created under
    /// (a person must not review their own access), targeting the campaign's escalation
    /// contact.
    Escalate {
        /// Who decides the item if the reviewer does not: a person, never the reviewer.
        to: String,
    },
    /// Remove every undecided item at the deadline, with the fixed reason
    /// `unreviewed at deadline`; the identity service applies the removal.
    RemoveUnreviewed,
    /// Do nothing at the deadline; the items stay open for a later campaign.
    LeaveOpen,
}

/// One reviewer's decision on one item. Decisions are final: after one lands, a different
/// decision needs a new campaign.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Decision {
    /// Keep the access. The note travels to the report when the item is stale or carries a
    /// SoD override, where a non-blank note is required (why the unused or overridden
    /// access is kept).
    Keep {
        /// Why the access is kept, when a reason is owed.
        note: Option<String>,
    },
    /// Remove the assignment. A non-blank reason is mandatory; the identity service
    /// receives a [`crate::ReviewAction::RemoveAssignment`] carrying it.
    Remove {
        /// Why the access is removed; audited verbatim.
        reason: String,
    },
}

/// The campaign setup: what is under review, who opened it, by when, and what the deadline
/// does.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CampaignSpec {
    /// The campaign id, stable in every event and report row.
    pub id: String,
    /// The human title, shown on the report.
    pub title: String,
    /// Who opened the campaign; the actor of the opening, escalation and
    /// deadline-removal events.
    pub opened_by: String,
    /// Who decides items when their own reviewer may not (a person must not review their
    /// own access): the escalation target an own-access item is created with. May be empty
    /// only when no item needs it.
    pub escalation_contact: String,
    /// The deadline, in the same integer epoch seconds as every other time in this crate.
    /// At or after it, undecided items follow [`CampaignSpec::on_deadline`].
    pub deadline: i64,
    /// How many days of non-use make an item stale: a `last_use` older than this (or
    /// absent) owes a non-blank note when kept. Never floating point (ADR-12).
    pub stale_after_days: i64,
    /// What happens to undecided items at the deadline.
    pub on_deadline: DeadlinePolicy,
}

/// One assignment under review, snapshotted by the caller from the identity data: the
/// assignment, its source, the user's last use of it and any SoD override. The campaign
/// stores the snapshot unchanged; "last use" comes from the audit log (the caller supplies
/// it), never read here.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewItem {
    /// The item id, unique within the campaign.
    pub id: String,
    /// The user whose access the item is about.
    pub user: String,
    /// The assignment the item reviews; returned unchanged in a removal action.
    pub assignment: AssignmentRef,
    /// Where the assignment came from (direct grant, duty, ...), as recorded by the caller.
    pub source: String,
    /// When the user last used the assignment; `None` is never-used, which is stale.
    pub last_use: Option<i64>,
    /// The recorded SoD override, if the assignment holds one.
    pub sod_override: Option<OverrideNote>,
    /// The reviewer who must decide the item. A person must not review their own access:
    /// an item whose reviewer equals its user is created already escalated to the
    /// campaign's escalation contact.
    pub reviewer: String,
}

impl ReviewItem {
    /// Whether the item is stale at `now`: never used, or last used more than
    /// `stale_after_days` days ago (86400 seconds a day, integer arithmetic only).
    #[must_use]
    pub fn is_stale(&self, stale_after_days: i64, now: i64) -> bool {
        match self.last_use {
            None => true,
            Some(last_use) => {
                let age = now - last_use;
                age > stale_after_days.saturating_mul(86_400)
            }
        }
    }

    /// Whether the item carries a recorded SoD override.
    #[must_use]
    pub fn is_overridden(&self) -> bool {
        self.sod_override.is_some()
    }
}

/// The per-item state after creation: pending (possibly escalated), or the final decision.
/// Public so the persisted campaign reads back as exactly what was stored.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewItemState {
    /// Awaiting a decision. `escalated_to` is set when the item's authority moved: an
    /// own-access item at creation, or the deadline policy. Only the named escalation
    /// target may decide then; the original reviewer may not.
    Pending {
        /// The person who may now decide, when escalated.
        escalated_to: Option<String>,
    },
    /// Kept by `by` at `at`, with the note they gave. The `stale` and `overridden` flags
    /// record what was true at decision time, so the report can list every kept stale or
    /// overridden item with its note.
    Kept {
        /// Who kept the item.
        by: String,
        /// The note they owed when the item was stale or overridden.
        note: Option<String>,
        /// When they decided, in epoch seconds.
        at: i64,
        /// Whether the item was stale at decision time.
        stale: bool,
        /// Whether the item carried a SoD override at decision time.
        overridden: bool,
    },
    /// Removed by `by` at `at` for `reason`; carried verbatim to the report and to the
    /// `RemoveAssignment` action for the identity service. Deadline removals record the
    /// campaign opener as `by` and the fixed reason `unreviewed at deadline`.
    Removed {
        /// Who removed the item.
        by: String,
        /// Why the access is removed, audited verbatim.
        reason: String,
        /// When it was removed, in epoch seconds.
        at: i64,
    },
}

impl ReviewItemState {
    /// Whether the item has reached its final state: a decision (kept or removed) is
    /// terminal; a pending item is not, escalated or not.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending { .. })
    }
}

/// Where the campaign is overall: open while any item awaits a decision (possibly past its
/// deadline, per the policy), completed once every item is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CampaignStatus {
    /// Items may still be decided (an escalation target may still act on an old item).
    Open,
    /// Every item is decided; nothing more can change.
    Completed,
}
