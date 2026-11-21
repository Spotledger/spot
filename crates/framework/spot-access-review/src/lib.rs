//! The periodic access review campaign (blueprint WS14, WP-SEC-02, ADR-070 section 8 rule 3):
//! the review workflow of a campaign as a pure state machine.
//!
//! # Shape
//!
//! - [`model`]: the data: [`CampaignSpec`](model::CampaignSpec) (what and by when),
//!   [`ReviewItem`](model::ReviewItem) (one assignment's snapshot, taken by the caller and
//!   stored unchanged), the [`DeadlinePolicy`](model::DeadlinePolicy) and the decisions.
//! - [`campaign`]: the state machine: [`Campaign::open`](campaign::Campaign::open),
//!   [`decide`](campaign::Campaign::decide) (keep or remove, by the item's reviewer only) and
//!   [`tick`](campaign::Campaign::tick) (the deadline policy, idempotent).
//! - [`event`]: the audit vocabulary: [`ReviewEvent`](event::ReviewEvent), journalled by the
//!   identity service on the `identity` topic; every event carries campaign id, item id,
//!   actor and time and never credentials or free text beyond the note and reason.
//! - [`report`]: [`ReviewReport`](report::ReviewReport) — the counts, per reviewer progress,
//!   removals with reasons and every kept stale or overridden item with its note — and
//!   [`to_csv`](report::ReviewReport::to_csv) for the auditor, with correct quoting.
//! - [`error`]: the one error type, with stable `review.*` codes that name the item and rule.
//!
//! # Rules (the behaviour this crate is, in our own words)
//!
//! - A person must not review their own access: an item whose reviewer equals its user is
//!   created already escalated to the campaign's escalation contact; with no contact the
//!   campaign cannot open.
//! - Only the item's current reviewer (or the escalation target after an escalation) may
//!   decide. Decisions are final: a different decision needs a new campaign.
//! - Keeping unused or overridden access needs a non-blank note; removing needs a non-blank
//!   reason. A removal yields a [`ReviewAction::RemoveAssignment`](event::ReviewAction) for
//!   the identity service; this crate never removes access itself.
//! - At or after the deadline each undecided item follows the tenant's
//!   [`DeadlinePolicy`](model::DeadlinePolicy): escalated (once), removed (reason
//!   `unreviewed at deadline`) or left open.
//!
//! # Persistence
//!
//! The state machine itself stores nothing. [`store`] is the port a host persists it through
//! ([`CampaignStore`](store::CampaignStore): tenant-scoped, compare-and-set decisions, an
//! append-only history), [`memory`] its in-process fake, and `contract` (feature `testing`)
//! the suite every backend must pass; `spot-access-review-pg` is the PostgreSQL backend.
//!
//! # Purity
//!
//! The state machine has no storage, no clock, no ambient reads, no floats, no `unsafe`, no `unwrap`/`expect`
//! outside tests (the project coding rules). Time comes in as an argument; results come out as
//! data ([`Outcome`](event::Outcome)).

pub mod campaign;
#[cfg(feature = "testing")]
pub mod contract;
pub mod error;
pub mod event;
pub mod memory;
pub mod model;
pub mod report;
pub mod store;

pub use campaign::Campaign;
pub use error::ReviewError;
pub use event::{DecisionRecord, Outcome, ReviewAction, ReviewEvent};
pub use memory::MemoryCampaignStore;
pub use model::{
    AssignmentRef, CampaignSpec, CampaignStatus, DeadlinePolicy, Decision, OverrideNote,
    ReviewItem, ReviewItemState,
};
pub use report::ReviewReport;
pub use store::{CampaignStore, StoreError};

/// The name of this crate, for diagnostics and the test matrix header.
pub const CRATE_NAME: &str = "spot-access-review";
