//! The audit vocabulary and the data a step returns. Every event carries the campaign id,
//! item id, actor and time, and never credentials, tokens or secrets; the only free text an
//! event carries is a decision's note or reason (ADR-070 section 8 rule 4: the identity
//! service journals these on the `identity` topic).

/// What the identity service should do with the results of a step. The campaign never
/// removes access itself: removals come out as actions for the service to apply.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewAction {
    /// Remove the assignment: the identity service applies it and journalls the audit
    /// rows.
    RemoveAssignment {
        /// The assignment to remove, unchanged from the snapshot.
        assignment: crate::model::AssignmentRef,
        /// Which campaign ordered it.
        campaign: String,
        /// Why, audited verbatim (`unreviewed at deadline` for a deadline removal).
        reason: String,
    },
}

/// What happened, as audit events plus the actions the identity service must apply. Every
/// method returns one; an empty outcome (an idempotent second [`crate::Campaign::tick`])
/// carries nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    /// The audit events, in the order they happened.
    pub events: Vec<ReviewEvent>,
    /// The removals the identity service must apply, in the same order as the events.
    pub actions: Vec<ReviewAction>,
}

impl Outcome {
    /// Whether nothing happened: no events and no actions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.actions.is_empty()
    }
}

/// One audit event. The variants are the campaign's vocabulary; nothing here carries
/// credentials or free text beyond a decision's note or reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewEvent {
    /// The campaign opened with its items ([`crate::Campaign::open`]).
    CampaignOpened {
        /// The campaign id.
        campaign: String,
        /// How many items it opened with.
        items: usize,
        /// Who opened it.
        by: String,
        /// When, in epoch seconds.
        at: i64,
    },

    /// An item was decided (kept or removed) by its reviewer or escalation target.
    Decided {
        /// The campaign id.
        campaign: String,
        /// The item.
        item: String,
        /// What was decided and by whom.
        decision: DecisionRecord,
        /// When, in epoch seconds.
        at: i64,
    },

    /// An item's authority moved to the escalation target: at creation (a person must not
    /// review their own access) or at the deadline under
    /// [`crate::DeadlinePolicy::Escalate`].
    Escalated {
        /// The campaign id.
        campaign: String,
        /// The item.
        item: String,
        /// Who the item was escalated to.
        to: String,
        /// Why (`own access` at creation, or the deadline).
        why: String,
        /// When, in epoch seconds.
        at: i64,
    },

    /// An item was removed at the deadline under [`crate::DeadlinePolicy::RemoveUnreviewed`]
    /// (and the `Removed` state was recorded); the matching
    /// [`ReviewAction::RemoveAssignment`] carries the reason.
    Removed {
        /// The campaign id.
        campaign: String,
        /// The item.
        item: String,
        /// Who ordered the removal (the campaign opener for a deadline removal).
        by: String,
        /// When, in epoch seconds.
        at: i64,
    },

    /// Every item is decided; the campaign is completed and closed, and reports its
    /// numbers.
    Completed {
        /// The campaign id.
        campaign: String,
        /// When, in epoch seconds.
        at: i64,
    },
}

/// The decision as the audit journal sees it: kept, with the note the tenant owes when the
/// access is stale or overridden, or removed with the reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DecisionRecord {
    /// The access was kept.
    Kept {
        /// Who decided.
        by: String,
        /// The note, when one was owed or given.
        note: Option<String>,
    },
    /// The access was removed.
    Removed {
        /// Who decided.
        by: String,
        /// The reason, audited verbatim.
        reason: String,
    },
}
