//! The persistence port of the access review campaign (WP-SEC-02.T3): the trait a backend
//! implements, its error, the bounds every backend enforces and the pure planning functions
//! every backend shares, so the memory fake and PostgreSQL cannot drift apart.
//!
//! # Rules every [`CampaignStore`] keeps
//!
//! - **Tenant first.** Every method takes the caller's [`TenantId`]. A campaign of another
//!   tenant answers [`StoreError::NotFound`], exactly like a campaign that does not exist.
//! - **Compare-and-set.** A decision names the item version the reviewer read. A stale
//!   version is [`StoreError::VersionConflict`]; of two reviewers racing for one item,
//!   exactly one wins.
//! - **No self-decision.** Nobody may decide an item about their own access, whatever the
//!   escalation produced ([`check_decide`]).
//! - **Append-only history.** Events, removal actions and refusals are appended in the same
//!   transaction as the state change and are never updated or deleted.
//! - **Snapshots are stored unchanged.** The items of a campaign are written once, at
//!   opening, and never recomputed: the review judges what the reviewer saw.
//! - **Fail closed.** A storage fault is [`StoreError::Unavailable`] with a fixed sentence;
//!   no driver text, identifier or note ever appears in an error.
//! - **Bounded.** Ids, texts, item counts and page sizes are limited ([`MAX_ID_BYTES`] and
//!   the other constants); an oversized input is [`StoreError::InputInvalid`], never
//!   truncated.
//! - **Time is an argument.** No store reads a clock.

use std::future::Future;
use std::pin::Pin;

use spot_tenancy::TenantId;

use crate::campaign::Campaign;
use crate::error::ReviewError;
use crate::event::{Outcome, ReviewAction, ReviewEvent};
use crate::model::{CampaignSpec, CampaignStatus, DeadlinePolicy, Decision, ReviewItem};

/// A boxed, sendable future, so [`CampaignStore`] can be used through `dyn` and needs no async
/// runtime in this crate.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The longest id or name (campaign, item, user, reviewer, actor), in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// The longest free text (title, note, reason, source, assignment reference), in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;
/// The most items one campaign may hold.
pub const MAX_ITEMS: usize = 10_000;
/// The most rows one page of a listing or of the history may hold.
pub const MAX_PAGE: u32 = 200;

/// Why a store call failed. Messages are fixed sentences: they never carry a driver message,
/// an identifier or a note.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The state machine refused: wrong reviewer, decided item, missing note, closed
    /// campaign. Carries its own stable `review.*` code.
    #[error(transparent)]
    Review(#[from] ReviewError),
    /// No such campaign in this tenant (also what another tenant's campaign answers).
    #[error("no such campaign")]
    NotFound,
    /// The tenant already has a campaign with this id.
    #[error("a campaign with this id already exists")]
    AlreadyExists,
    /// The item changed since the caller read it.
    #[error("the item changed since it was read; reload it and decide again")]
    VersionConflict,
    /// An input is out of bounds; the text names the field class, never the value.
    #[error("input out of bounds: {0}")]
    InputInvalid(&'static str),
    /// The stored state contradicts itself and was refused.
    #[error("the stored campaign is inconsistent and was refused")]
    Corrupt,
    /// The storage could not answer; nothing was changed.
    #[error("the campaign store is unavailable")]
    Unavailable,
}

/// The stable code of [`StoreError::NotFound`].
pub const NOT_FOUND: &str = "access_review_pg.not_found";
/// The stable code of [`StoreError::AlreadyExists`].
pub const ALREADY_EXISTS: &str = "access_review_pg.already_exists";
/// The stable code of [`StoreError::VersionConflict`].
pub const VERSION_CONFLICT: &str = "access_review_pg.version_conflict";
/// The stable code of [`StoreError::InputInvalid`].
pub const INPUT_INVALID: &str = "access_review_pg.input_invalid";
/// The stable code of [`StoreError::Corrupt`].
pub const CORRUPT: &str = "access_review_pg.corrupt_state";
/// The stable code of [`StoreError::Unavailable`].
pub const UNAVAILABLE: &str = "access_review_pg.unavailable";

impl StoreError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Review(e) => e.code(),
            Self::NotFound => NOT_FOUND,
            Self::AlreadyExists => ALREADY_EXISTS,
            Self::VersionConflict => VERSION_CONFLICT,
            Self::InputInvalid(_) => INPUT_INVALID,
            Self::Corrupt => CORRUPT,
            Self::Unavailable => UNAVAILABLE,
        }
    }
}

/// A campaign as stored: the state machine plus the version of each item (parallel to
/// [`Campaign::items`]), which a decision must quote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCampaign {
    /// The campaign, read back identical to what was persisted.
    pub campaign: Campaign,
    /// The version of each item: 0 at opening, plus one for every state change.
    pub item_versions: Vec<u64>,
}

/// One row of a campaign listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CampaignSummary {
    /// The campaign id.
    pub id: String,
    /// The title.
    pub title: String,
    /// Open or completed.
    pub status: CampaignStatus,
    /// The deadline, epoch seconds.
    pub deadline: i64,
    /// How many items the campaign holds.
    pub items: usize,
}

/// One entry of the append-only history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HistoryEntry {
    /// An audit event the state machine produced.
    Event(ReviewEvent),
    /// A removal action for the identity service. Recorded so that a host which crashed
    /// after the state change can still find what it owes.
    Action(ReviewAction),
    /// A refused decision: who tried, on which item, and the stable code. Never a note.
    Refused {
        /// The item the decision named.
        item: String,
        /// Who tried.
        by: String,
        /// The stable code of the refusal.
        code: String,
    },
}

impl HistoryEntry {
    /// The kind label stored next to the entry.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Event(_) => "event",
            Self::Action(_) => "action",
            Self::Refused { .. } => "refused",
        }
    }
}

/// A history entry with its position and time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRecord {
    /// The position: strictly increasing within a campaign, never reused.
    pub seq: u64,
    /// When the entry was written, epoch seconds (the caller's `now`).
    pub at: i64,
    /// The entry.
    pub entry: HistoryEntry,
}

/// The persistence port of a campaign. Every method is tenant-scoped and fallible.
pub trait CampaignStore: Send + Sync {
    /// Stores a new campaign: validates it through [`Campaign::open`], then writes the
    /// snapshot, the states and the opening events together.
    ///
    /// # Errors
    /// [`StoreError::AlreadyExists`] for a reused id, [`StoreError::InputInvalid`] for an
    /// out-of-bounds input, [`StoreError::Review`] for a spec the state machine refuses.
    fn open<'a>(
        &'a self,
        tenant: &'a TenantId,
        spec: CampaignSpec,
        items: Vec<ReviewItem>,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>>;

    /// Reads a campaign with its item versions.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] (also for another tenant's), [`StoreError::Corrupt`] when
    /// the stored state does not form a campaign.
    fn load<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
    ) -> StoreFuture<'a, Result<StoredCampaign, StoreError>>;

    /// Decides one item, if `expected_version` is the item's current version.
    ///
    /// # Errors
    /// [`StoreError::VersionConflict`] for a stale version, [`StoreError::Review`] when the
    /// state machine or the no-self-decision rule refuses, [`StoreError::NotFound`].
    #[allow(clippy::too_many_arguments)]
    fn decide<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        item: &'a str,
        by: &'a str,
        decision: Decision,
        expected_version: u64,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>>;

    /// Runs the deadline policy as one stored-state operation. Idempotent across callers and
    /// restarts: whatever a first tick changed, a second finds already done and returns an
    /// empty outcome.
    ///
    /// # Errors
    /// [`StoreError::NotFound`], [`StoreError::Corrupt`], [`StoreError::Unavailable`].
    fn tick<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>>;

    /// Lists the tenant's campaigns in id order (byte order), after `after`, at most `limit`.
    ///
    /// # Errors
    /// [`StoreError::InputInvalid`] for a limit of 0 or above [`MAX_PAGE`].
    fn list<'a>(
        &'a self,
        tenant: &'a TenantId,
        after: Option<&'a str>,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<CampaignSummary>, StoreError>>;

    /// Reads the campaign's history after position `after_seq`, oldest first, at most `limit`.
    ///
    /// # Errors
    /// [`StoreError::NotFound`], [`StoreError::InputInvalid`] for a bad limit.
    fn history<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        after_seq: u64,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<HistoryRecord>, StoreError>>;
}

fn bounded(text: &str, max: usize, what: &'static str) -> Result<(), StoreError> {
    if text.len() > max {
        Err(StoreError::InputInvalid(what))
    } else {
        Ok(())
    }
}

/// Checks a page size: 1 to [`MAX_PAGE`].
///
/// # Errors
/// [`StoreError::InputInvalid`] outside that range.
pub fn check_limit(limit: u32) -> Result<(), StoreError> {
    if limit == 0 || limit > MAX_PAGE {
        Err(StoreError::InputInvalid("page size"))
    } else {
        Ok(())
    }
}

/// Checks the lookup key of a campaign.
///
/// # Errors
/// [`StoreError::InputInvalid`] for an id longer than [`MAX_ID_BYTES`].
pub fn check_id(id: &str) -> Result<(), StoreError> {
    bounded(id, MAX_ID_BYTES, "campaign id")
}

/// Checks every size bound of an opening. The state machine's own rules (a blank id, a
/// duplicate item) stay in [`Campaign::open`].
///
/// # Errors
/// [`StoreError::InputInvalid`] naming the field class.
pub fn check_open(spec: &CampaignSpec, items: &[ReviewItem]) -> Result<(), StoreError> {
    bounded(&spec.id, MAX_ID_BYTES, "campaign id")?;
    bounded(&spec.title, MAX_TEXT_BYTES, "campaign title")?;
    bounded(&spec.opened_by, MAX_ID_BYTES, "opener")?;
    bounded(&spec.escalation_contact, MAX_ID_BYTES, "escalation contact")?;
    if let DeadlinePolicy::Escalate { to } = &spec.on_deadline {
        bounded(to, MAX_ID_BYTES, "escalation target")?;
    }
    if items.len() > MAX_ITEMS {
        return Err(StoreError::InputInvalid("item count"));
    }
    for item in items {
        bounded(&item.id, MAX_ID_BYTES, "item id")?;
        bounded(&item.user, MAX_ID_BYTES, "item user")?;
        bounded(&item.reviewer, MAX_ID_BYTES, "item reviewer")?;
        bounded(item.assignment.as_str(), MAX_TEXT_BYTES, "assignment")?;
        bounded(&item.source, MAX_TEXT_BYTES, "item source")?;
        if let Some(note) = &item.sod_override {
            bounded(note.as_str(), MAX_TEXT_BYTES, "override note")?;
        }
    }
    Ok(())
}

/// Checks the bounds of a decision and the rule no state machine state can express: nobody
/// decides an item about their own access, even when an escalation named them.
///
/// # Errors
/// [`StoreError::InputInvalid`] for oversize input, [`StoreError::Review`] with
/// `review.not_authorized` for a self-decision.
pub fn check_decide(
    campaign: &Campaign,
    item: &str,
    by: &str,
    decision: &Decision,
) -> Result<(), StoreError> {
    bounded(item, MAX_ID_BYTES, "item id")?;
    bounded(by, MAX_ID_BYTES, "actor")?;
    match decision {
        Decision::Keep { note: Some(note) } => bounded(note, MAX_TEXT_BYTES, "note")?,
        Decision::Remove { reason } => bounded(reason, MAX_TEXT_BYTES, "reason")?,
        Decision::Keep { note: None } => {}
    }
    if campaign
        .items()
        .iter()
        .any(|i| i.id == item && i.user == by)
    {
        return Err(StoreError::Review(ReviewError::NotAuthorized {
            item: item.to_string(),
            detail: "a person may not decide their own access".to_string(),
        }));
    }
    Ok(())
}

/// The result of planning a step: the campaign after it, what to write and what to return.
#[derive(Clone, Debug)]
pub struct Transition {
    /// The campaign after the step.
    pub campaign: Campaign,
    /// The item versions after the step.
    pub versions: Vec<u64>,
    /// The indexes of the items whose state changed (each gained one version).
    pub changed: Vec<usize>,
    /// What to append to the history, in order.
    pub entries: Vec<HistoryEntry>,
    /// What to return to the caller.
    pub outcome: Outcome,
}

/// The history entries of an outcome: its events, then its actions.
#[must_use]
pub fn entries_of(outcome: &Outcome) -> Vec<HistoryEntry> {
    outcome
        .events
        .iter()
        .cloned()
        .map(HistoryEntry::Event)
        .chain(outcome.actions.iter().cloned().map(HistoryEntry::Action))
        .collect()
}

fn diff(before: &Campaign, after: &Campaign, versions: &[u64]) -> (Vec<usize>, Vec<u64>) {
    let changed: Vec<usize> = before
        .item_states()
        .iter()
        .zip(after.item_states())
        .enumerate()
        .filter_map(|(i, (a, b))| (a != b).then_some(i))
        .collect();
    let mut next = versions.to_vec();
    for &i in &changed {
        if let Some(v) = next.get_mut(i) {
            *v = v.saturating_add(1);
        }
    }
    (changed, next)
}

/// Plans a decision on the stored campaign: the version check first (a stale reader never
/// learns anything about the item's state), then the bounds and the self-decision rule, then
/// the state machine.
///
/// # Errors
/// [`StoreError::VersionConflict`], [`StoreError::InputInvalid`], [`StoreError::Review`].
pub fn plan_decide(
    stored: &StoredCampaign,
    item: &str,
    by: &str,
    decision: Decision,
    expected_version: u64,
    now: i64,
) -> Result<Transition, StoreError> {
    check_decide(&stored.campaign, item, by, &decision)?;
    if let Some(idx) = stored.campaign.items().iter().position(|i| i.id == item)
        && stored.item_versions.get(idx).copied() != Some(expected_version)
    {
        return Err(StoreError::VersionConflict);
    }
    let mut campaign = stored.campaign.clone();
    let outcome = campaign.decide(item, by, decision, now)?;
    let (changed, versions) = diff(&stored.campaign, &campaign, &stored.item_versions);
    Ok(Transition {
        entries: entries_of(&outcome),
        campaign,
        versions,
        changed,
        outcome,
    })
}

/// Plans the deadline policy on the stored campaign. Pure, and idempotent by construction:
/// planning again on the result changes nothing.
#[must_use]
pub fn plan_tick(stored: &StoredCampaign, now: i64) -> Transition {
    let mut campaign = stored.campaign.clone();
    let outcome = campaign.tick(now);
    let (changed, versions) = diff(&stored.campaign, &campaign, &stored.item_versions);
    Transition {
        entries: entries_of(&outcome),
        campaign,
        versions,
        changed,
        outcome,
    }
}

/// The refusal record for a failed decision, when the failure is one worth auditing and the
/// campaign is known to exist (a refusal is attributed; an unknown campaign has nothing to
/// attach it to, and recording it would let a caller grow the history without bound).
#[must_use]
pub fn refusal_entry(error: &StoreError, item: &str, by: &str) -> Option<HistoryEntry> {
    match error {
        StoreError::Review(_) | StoreError::VersionConflict => Some(HistoryEntry::Refused {
            item: item.to_string(),
            by: by.to_string(),
            code: error.code().to_string(),
        }),
        _ => None,
    }
}
