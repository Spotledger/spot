//! The state machine: open, decide, tick. Time comes in as an argument, results come out
//! as data; no clock, no storage, no floats (the project coding rules, ADR-12).

use crate::error::ReviewError;
use crate::event::{DecisionRecord, Outcome, ReviewAction, ReviewEvent};
use crate::model::{
    CampaignSpec, CampaignStatus, DeadlinePolicy, Decision, ReviewItem, ReviewItemState,
};
use crate::report::ReviewReport;

/// The fixed reason every deadline removal carries (ADR-070 section 8 rule 3: unreviewed
/// items removed at the deadline are removed for that reason, audited verbatim).
pub const UNREVIEWED_AT_DEADLINE: &str = "unreviewed at deadline";

/// One reviewer's load and progress, for [`ReviewReport`] and its CSV.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewerProgress {
    /// The reviewer's name as the items name them.
    pub reviewer: String,
    /// How many items were assigned to them at opening.
    pub total: usize,
    /// How many of their items have reached a final state — by their own decision, the
    /// escalation target's, or the deadline policy.
    pub decided: usize,
    /// How many of their items still await a decision.
    pub pending: usize,
}

/// An access review campaign: the snapshot, the per-item state, the deadline policy and
/// the report. Create with [`Campaign::open`] — the validation gate — then decide or tick;
/// every step returns an [`Outcome`] of audit events and removal actions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Campaign {
    spec: CampaignSpec,
    /// The snapshot, in input order, stored unchanged.
    items: Vec<ReviewItem>,
    /// The per-item state, parallel to `items` by construction.
    state: Vec<ReviewItemState>,
    /// The events the opening recorded: escalations of own-access items, then the opening.
    open_event: Vec<ReviewEvent>,
    status: CampaignStatus,
}

impl Campaign {
    /// Opens a campaign: validates spec and items, escalates own-access items at creation
    /// (a person must not review their own access) to the campaign's escalation contact,
    /// then records the opening.
    ///
    /// # Errors
    ///
    /// Errors name the rule that was broken ([`ReviewError::SpecInvalid`] or
    /// [`ReviewError::ItemInvalid`]): no items, a deadline already past, blank spec
    /// fields, a negative staleness, an escalation with an empty target, an own-access
    /// item with an empty escalation contact, a blank item id or assignment reference, or
    /// a duplicate item id.
    pub fn open(spec: CampaignSpec, items: Vec<ReviewItem>, now: i64) -> Result<Self, ReviewError> {
        let spec_err = |detail: &str| ReviewError::SpecInvalid {
            detail: detail.to_string(),
        };
        if items.is_empty() {
            return Err(spec_err("a campaign opens with at least one item"));
        }
        if spec.deadline < now {
            return Err(spec_err(
                "the deadline is already past when the campaign opens",
            ));
        }
        if spec.id.trim().is_empty() {
            return Err(spec_err("the campaign id is missing"));
        }
        if spec.title.trim().is_empty() {
            return Err(spec_err("the campaign title is missing"));
        }
        if spec.opened_by.trim().is_empty() {
            return Err(spec_err("the campaign opener is missing"));
        }
        if spec.stale_after_days < 0 {
            return Err(spec_err("stale_after_days may not be negative"));
        }
        let own_access = items.iter().any(|i| i.reviewer == i.user);
        if own_access && spec.escalation_contact.trim().is_empty() {
            return Err(spec_err(
                "an item's reviewer is its user and the escalation contact is empty",
            ));
        }
        if matches!(spec.on_deadline, DeadlinePolicy::Escalate { ref to } if to.trim().is_empty()) {
            return Err(spec_err("the deadline policy escalates to an empty target"));
        }

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for item in &items {
            if item.id.trim().is_empty() {
                return Err(ReviewError::ItemInvalid {
                    item: item.id.clone(),
                    detail: "the item id is blank".to_string(),
                });
            }
            if !seen.insert(item.id.as_str()) {
                return Err(ReviewError::ItemInvalid {
                    item: item.id.clone(),
                    detail: "the item id is a duplicate".to_string(),
                });
            }
            if item.assignment.as_str().trim().is_empty() {
                return Err(ReviewError::ItemInvalid {
                    item: item.id.clone(),
                    detail: "the assignment reference is blank".to_string(),
                });
            }
        }

        // The state starts parallel to the snapshot: own-access items are created already
        // escalated to the campaign's escalation contact.
        let state = items
            .iter()
            .map(|i| {
                if i.reviewer == i.user {
                    ReviewItemState::Pending {
                        escalated_to: Some(spec.escalation_contact.clone()),
                    }
                } else {
                    ReviewItemState::Pending { escalated_to: None }
                }
            })
            .collect();

        let mut campaign = Self {
            spec,
            items,
            state,
            open_event: Vec::new(),
            status: CampaignStatus::Open,
        };
        let mut events = Vec::new();
        for item in campaign.items.iter().filter(|i| i.reviewer == i.user) {
            events.push(ReviewEvent::Escalated {
                campaign: campaign.spec.id.clone(),
                item: item.id.clone(),
                to: campaign.spec.escalation_contact.clone(),
                why: "own access".to_string(),
                at: now,
            });
        }
        events.push(ReviewEvent::CampaignOpened {
            campaign: campaign.spec.id.clone(),
            items: campaign.items.len(),
            by: campaign.spec.opened_by.clone(),
            at: now,
        });
        campaign.open_event = events;
        Ok(campaign)
    }

    /// The audit events the opening recorded: one `Escalated` per own-access item, then
    /// `CampaignOpened`. The identity service journals them on the `identity` topic.
    #[must_use]
    pub fn opening_events(&self) -> &[ReviewEvent] {
        &self.open_event
    }

    /// The campaign spec.
    #[must_use]
    pub fn spec(&self) -> &CampaignSpec {
        &self.spec
    }

    /// The snapshot items, in input order, unchanged from what the caller took.
    #[must_use]
    pub fn items(&self) -> &[ReviewItem] {
        &self.items
    }

    /// The per-item state, parallel to [`Campaign::items`].
    #[must_use]
    pub fn item_states(&self) -> &[ReviewItemState] {
        &self.state
    }

    /// Whether every item is decided; then the campaign is closed and nothing can change.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status == CampaignStatus::Completed
    }

    fn item_index(&self, item_id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.id == item_id)
    }

    /// Who may decide the item now: the reviewer on a plain pending item, the escalation
    /// target on an escalated one, and nobody on a decided item.
    fn authorized(&self, idx: usize, by: &str) -> bool {
        match &self.state[idx] {
            ReviewItemState::Pending { escalated_to } => match escalated_to {
                None => self.items[idx].reviewer == by,
                Some(target) => target == by,
            },
            _ => false,
        }
    }

    fn invalid(&self, item_id: &str, detail: &str) -> ReviewError {
        ReviewError::DecisionInvalid {
            item: item_id.to_string(),
            detail: detail.to_string(),
        }
    }

    /// Decides one item. Only the item's current reviewer — or the escalation target on an
    /// escalated item — may decide; decisions are final, so a decided item refuses again
    /// and the completed campaign refuses everything.
    ///
    /// # Errors
    ///
    /// - the campaign is completed: [`ReviewError::CampaignClosed`];
    /// - the item does not exist or is already decided: [`ReviewError::DecisionInvalid`];
    /// - the actor may not decide this item: [`ReviewError::NotAuthorized`];
    /// - the decision breaks its rule — a removal without a non-blank reason, or a keep of
    ///   stale or overridden access without a non-blank note:
    ///   [`ReviewError::DecisionInvalid`].
    pub fn decide(
        &mut self,
        item_id: &str,
        by: &str,
        decision: Decision,
        now: i64,
    ) -> Result<Outcome, ReviewError> {
        if self.is_completed() {
            return Err(ReviewError::CampaignClosed);
        }
        let Some(idx) = self.item_index(item_id) else {
            return Err(self.invalid(item_id, "no such item in this campaign"));
        };
        if !self.authorized(idx, by) {
            let detail = match &self.state[idx] {
                ReviewItemState::Pending { escalated_to } => match escalated_to {
                    Some(target) => {
                        format!("only {target} may decide this escalated item")
                    }
                    None => format!("only {} may decide this item", self.items[idx].reviewer),
                },
                _ => "the item is already decided; decisions are final".to_string(),
            };
            return Err(ReviewError::NotAuthorized {
                item: item_id.to_string(),
                detail,
            });
        }

        let is_stale = self.items[idx].is_stale(self.spec.stale_after_days, now);
        let is_overridden = self.items[idx].is_overridden();
        match &decision {
            Decision::Remove { reason } => {
                if reason.trim().is_empty() {
                    return Err(self.invalid(item_id, "a removal needs a non-blank reason"));
                }
            }
            Decision::Keep { note } => {
                let noted = note.as_ref().is_some_and(|n| !n.trim().is_empty());
                if (is_stale || is_overridden) && !noted {
                    return Err(self.invalid(
                        item_id,
                        "keeping stale or overridden access needs a non-blank note",
                    ));
                }
            }
        }

        let (event, action) = match &decision {
            Decision::Keep { note } => {
                self.state[idx] = ReviewItemState::Kept {
                    by: by.to_string(),
                    note: note.clone(),
                    at: now,
                    stale: is_stale,
                    overridden: is_overridden,
                };
                (
                    ReviewEvent::Decided {
                        campaign: self.spec.id.clone(),
                        item: item_id.to_string(),
                        decision: DecisionRecord::Kept {
                            by: by.to_string(),
                            note: note.clone(),
                        },
                        at: now,
                    },
                    None,
                )
            }
            Decision::Remove { reason } => {
                let reason = reason.clone();
                self.state[idx] = ReviewItemState::Removed {
                    by: by.to_string(),
                    reason: reason.clone(),
                    at: now,
                };
                (
                    ReviewEvent::Decided {
                        campaign: self.spec.id.clone(),
                        item: item_id.to_string(),
                        decision: DecisionRecord::Removed {
                            by: by.to_string(),
                            reason: reason.clone(),
                        },
                        at: now,
                    },
                    Some(ReviewAction::RemoveAssignment {
                        assignment: self.items[idx].assignment.clone(),
                        campaign: self.spec.id.clone(),
                        reason,
                    }),
                )
            }
        };
        let mut events = vec![event];
        let actions = action.into_iter().collect();
        if let Some(completed) = self.complete_if_done(now) {
            events.push(completed);
        }
        Ok(Outcome { events, actions })
    }

    /// Runs the deadline policy over the undecided items. Idempotent: a second
    /// [`Campaign::tick`] at or after the deadline changes nothing further — escalated
    /// items escalate once, removed items stay removed.
    ///
    /// - [`DeadlinePolicy::Escalate`]: each undecided item escalates once to the target;
    ///   that person may then decide, the original reviewer may not.
    /// - [`DeadlinePolicy::RemoveUnreviewed`]: each undecided item is removed with reason
    ///   `unreviewed at deadline` and a [`ReviewAction::RemoveAssignment`] for the identity
    ///   service.
    /// - [`DeadlinePolicy::LeaveOpen`]: nothing changes here.
    ///
    /// The campaign completes only when every item is decided, whatever the policy.
    #[must_use]
    pub fn tick(&mut self, now: i64) -> Outcome {
        let mut events = Vec::new();
        let mut actions = Vec::new();
        if self.is_completed() || now < self.spec.deadline {
            return Outcome { events, actions };
        }
        match self.spec.on_deadline.clone() {
            DeadlinePolicy::Escalate { to } => {
                for idx in 0..self.items.len() {
                    if let ReviewItemState::Pending { escalated_to: None } = self.state[idx] {
                        self.state[idx] = ReviewItemState::Pending {
                            escalated_to: Some(to.clone()),
                        };
                        events.push(ReviewEvent::Escalated {
                            campaign: self.spec.id.clone(),
                            item: self.items[idx].id.clone(),
                            to: to.clone(),
                            why: "unreviewed at deadline".to_string(),
                            at: now,
                        });
                    }
                }
            }
            DeadlinePolicy::RemoveUnreviewed => {
                for idx in 0..self.items.len() {
                    if let ReviewItemState::Pending { escalated_to: _ } = self.state[idx] {
                        let item = self.items[idx].clone();
                        self.state[idx] = ReviewItemState::Removed {
                            by: self.spec.opened_by.clone(),
                            reason: UNREVIEWED_AT_DEADLINE.to_string(),
                            at: now,
                        };
                        events.push(ReviewEvent::Removed {
                            campaign: self.spec.id.clone(),
                            item: item.id.clone(),
                            by: self.spec.opened_by.clone(),
                            at: now,
                        });
                        actions.push(ReviewAction::RemoveAssignment {
                            assignment: item.assignment,
                            campaign: self.spec.id.clone(),
                            reason: UNREVIEWED_AT_DEADLINE.to_string(),
                        });
                    }
                }
            }
            DeadlinePolicy::LeaveOpen => {}
        }
        if let Some(completed) = self.complete_if_done(now) {
            events.push(completed);
        }
        Outcome { events, actions }
    }

    /// Completes the campaign when every item is decided and returns the completion event
    /// to append to the outcome.
    fn complete_if_done(&mut self, at: i64) -> Option<ReviewEvent> {
        if self.status == CampaignStatus::Open
            && self.state.iter().all(ReviewItemState::is_terminal)
        {
            self.status = CampaignStatus::Completed;
            return Some(ReviewEvent::Completed {
                campaign: self.spec.id.clone(),
                at,
            });
        }
        None
    }

    /// Rebuilds a campaign from the parts a store persisted: the snapshot, the per-item
    /// state, the opening events and the status. The stored parts are checked, not trusted:
    /// a store that was edited by hand or corrupted must not come back as a campaign that
    /// the state machine would then act on.
    ///
    /// # Errors
    ///
    /// [`ReviewError::SpecInvalid`] when the parts contradict each other: no items, the
    /// state not parallel to the items, a blank or duplicate item id or assignment, or a
    /// status that does not match the states (completed exactly when every item is decided).
    pub fn restore(
        spec: CampaignSpec,
        items: Vec<ReviewItem>,
        state: Vec<ReviewItemState>,
        open_event: Vec<ReviewEvent>,
        status: CampaignStatus,
    ) -> Result<Self, ReviewError> {
        let bad = |detail: &str| ReviewError::SpecInvalid {
            detail: detail.to_string(),
        };
        if items.is_empty() {
            return Err(bad("a stored campaign has at least one item"));
        }
        if items.len() != state.len() {
            return Err(bad("the stored item states do not match the items"));
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for item in &items {
            if item.id.trim().is_empty()
                || item.assignment.as_str().trim().is_empty()
                || !seen.insert(item.id.as_str())
            {
                return Err(bad(
                    "a stored item has a blank or duplicate id or assignment",
                ));
            }
        }
        let all_decided = state.iter().all(ReviewItemState::is_terminal);
        if all_decided != (status == CampaignStatus::Completed) {
            return Err(bad("the stored status does not match the item states"));
        }
        Ok(Self {
            spec,
            items,
            state,
            open_event,
            status,
        })
    }

    /// The open/completed status, for tests and the service's queries.
    #[must_use]
    pub fn status(&self) -> CampaignStatus {
        self.status
    }

    /// The report: counts, per reviewer progress, removals with reasons, and every kept
    /// item that was stale or overridden with its note, all in deterministic order.
    #[must_use]
    pub fn report(&self) -> ReviewReport {
        ReviewReport::build(&self.spec, &self.items, &self.state, self.status)
    }
}
