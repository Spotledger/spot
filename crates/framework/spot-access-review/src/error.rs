//! The one error type, with stable `review.*` codes that name which item and which rule
//! failed. Codes follow the house pattern ([`crate::CRATE_NAME`]: permissions uses the same
//! shape with `perm.*`); the catalogue entries are delivered through
//! `crates/framework/spot-errors/catalogue/review.toml` (this task's owned paths exclude
//! the catalogue directory; see the task report).

/// Something failed while opening, deciding or ticking a review campaign.
///
/// Variants carry what went wrong in words, naming the item where one is involved; for
/// logs, metrics and API responses use [`ReviewError::code`]. Codes are stable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReviewError {
    /// The campaign spec contradicts itself: a deadline in the past, or no escalation
    /// contact where an own-access item or the policy needs one.
    #[error("{detail}")]
    SpecInvalid {
        /// What is wrong.
        detail: String,
    },

    /// An item is not reviewable: a blank id, a blank assignment reference, or the same
    /// id twice.
    #[error("item {item}: {detail}")]
    ItemInvalid {
        /// The item id, when known.
        item: String,
        /// What is wrong.
        detail: String,
    },

    /// The campaign is not open: it is completed, so no decision or escalation can change
    /// anything (decisions are final; a different decision needs a new campaign).
    #[error("the campaign is completed and cannot be changed")]
    CampaignClosed,

    /// Nobody named on the item may act on it: the wrong reviewer on a pending item, or
    /// anybody but the escalation target on an escalated one.
    #[error("item {item}: {detail}")]
    NotAuthorized {
        /// The item id.
        item: String,
        /// Who may act instead.
        detail: String,
    },

    /// The decision breaks its rule: the item does not exist, is already decided, or the
    /// keep/remove note or reason is owed and missing.
    #[error("item {item}: {detail}")]
    DecisionInvalid {
        /// The item id.
        item: String,
        /// Which rule the decision broke.
        detail: String,
    },
}

impl ReviewError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SpecInvalid { .. } => SPEC_INVALID,
            Self::ItemInvalid { .. } => ITEM_INVALID,
            Self::CampaignClosed => CAMPAIGN_CLOSED,
            Self::NotAuthorized { .. } => NOT_AUTHORIZED,
            Self::DecisionInvalid { .. } => DECISION_INVALID,
        }
    }
}

/// The campaign spec contradicts itself (a past deadline, a missing escalation contact).
pub const SPEC_INVALID: &str = "review.spec_invalid";
/// An item is not reviewable (a blank id or assignment reference, a duplicate id).
pub const ITEM_INVALID: &str = "review.item_invalid";
/// The campaign is completed; decisions are final and open items cannot exist.
pub const CAMPAIGN_CLOSED: &str = "review.campaign_closed";
/// The actor may not decide the item (wrong reviewer, or not the escalation target).
pub const NOT_AUTHORIZED: &str = "review.not_authorized";
/// The decision breaks its rule (unknown or decided item, an owed note or reason missing).
pub const DECISION_INVALID: &str = "review.decision_invalid";
