//! Shared-balance policies for balances several offline nodes can spend (WP-EDG-09.T1, ADR-062).
//!
//! A gift card, a customer credit line or a loyalty pot is one balance that every branch can
//! spend, and a branch can lose its connection to head office. What may a branch do then? That
//! is a business decision, and it is **the architecture owner's and is not yet made**. This crate builds all three
//! answers behind one trait, [`BalancePolicy`], chosen per balance type in a [`PolicyTable`], so
//! the decision later only picks defaults. The default, [`PROVISIONAL_DEFAULT`], is
//! [`PolicyKind::OnlineOnly`], the safest, and is labelled provisional.
//!
//! # The three policies, in plain language
//!
//! Take a 1,000.00 gift card and 50 branches.
//!
//! | | Online only ([`OnlineOnly`]) | Allocated quota ([`AllocatedQuota`]) | Accept and reconcile ([`AcceptAndReconcile`]) |
//! | --- | --- | --- | --- |
//! | Offline branch can sell on the card | No | Yes, up to the slice it was lent | Yes, up to its own cap |
//! | Card can be overspent | Never | Never | Yes, by at most `cap x branches` |
//! | Cost | Customer is turned away offline | Lent slices are tied up; an unreported slice stays reserved | A bounded loss that head office must chase |
//! | Who carries the risk | Nobody | Nobody | The business, up to the stated bound |
//! | Fits | High-value, low-trust, or regulated balances | Predictable branches that can be sized in advance | Low-value balances where refusing a sale costs more than the risk |
//!
//! Each policy states its limit as numbers through [`BalancePolicy::limits`], and the chaos
//! tests assert that limit under random offline periods, duplicate deliveries and restarts.
//!
//! # How it fits together
//!
//! - [`Ledger`]: a balance is the sum of append-only [`Entry`] values (credit, debit,
//!   adjustment) in integer minor units. An entry id is `(node, node sequence)`, so a
//!   redelivered entry is recognised and never applied twice.
//! - [`Hub`]: head office, the owner of every ledger, of the quota [`Grant`] values and of the
//!   [`Receivable`] claims.
//! - [`NodeView`]: one branch's persisted state: its entry sequence, its last view of each
//!   balance, its quota, and the entries head office has not yet acknowledged.
//! - A spend is [`BalancePolicy::authorise`] on the node. Reporting in is
//!   [`BalancePolicy::settle`] on the hub with a [`Batch`]; the node then applies the
//!   [`HubReply`] it gets back with [`NodeView::apply_hub_reply`], which acknowledges what head
//!   office handled **and** learns the totals the reply carries. Those two cannot be separated,
//!   so a node cannot keep spending against a stale, higher balance.
//! - Head office stamps each entry with its own receive time when it first sees it, and judges
//!   quota expiry on that time — never on the entry's own `at`, which is the node's word and
//!   would let a backdated entry dodge expiry.
//! - An entry head office refuses on arrival goes to a [`QuarantineSink`] with a
//!   [`RejectReason`]; it is never dropped, and a redelivery of it is not quarantined twice. The
//!   hub's mark on a refused entry has two states ([`Mark`]): *offered* when it hands the entry to
//!   the sink, *held* only when the sink confirms custody. A redelivery of an entry that is only
//!   offered is completed into the queue — never applied as well — so an entry id is either
//!   applied or queued, never both. A sink that fails ([`SinkError`]) leaves the entry
//!   in [`Reconciliation::deferred`], unacknowledged.
//! - The marks are state a host must keep: [`RefusalStore`] is the store port (one transaction per
//!   change, mark and reason together), [`Hub::refusals`] is what it writes,
//!   [`Hub::restore_refusals`] rebuilds it on start so [`Hub::offered_unconfirmed`] can re-offer
//!   what the queue never confirmed, and the `testing` feature ships the contract suite any
//!   durable store must pass.
//!
//! # What it never does
//!
//! It never reads the wall clock (time is an argument), never uses a float for money, quantity or
//! time, never panics on bad input, and never logs or returns a secret.
//!
//! # Policy is elsewhere
//!
//! ADRs 063 to 067 are still proposed, so the pieces they will decide sit behind seams: the
//! live link ([`Confirmer`]), the quarantine queue ([`QuarantineSink`]) and the choice of
//! policy ([`PolicyTable`]). Where a later ADR will matter is listed in the PR's open questions.

#![warn(missing_docs)]

pub mod accept;
pub mod codes;
pub mod error;
pub mod hub;
pub mod ledger;
pub mod node;
pub mod online;
pub mod policy;
pub mod quarantine;
pub mod quota;
pub mod reply;
pub mod store;
#[cfg(feature = "testing")]
pub mod testing;

pub use accept::AcceptAndReconcile;
pub use error::BalanceError;
pub use hub::{Grant, Hub, Mark, Receivable, Stranded};
pub use ledger::{Appended, BalanceId, Entry, EntryId, EntryKind, Ledger, Millis, NodeId, Sign};
pub use node::{NodeView, Quota};
pub use online::{Confirmation, Confirmer, OnlineOnly};
pub use policy::{
    BalancePolicy, Batch, Decision, DeclineReason, Limits, PROVISIONAL_DEFAULT, PolicyKind,
    PolicyTable, Reconciliation,
};
pub use quarantine::{QuarantineSink, RejectReason, SinkError, VecQuarantine};
pub use quota::AllocatedQuota;
pub use reply::{HubReply, HubTotal};
pub use store::{MemoryRefusalStore, RefusalRecord, RefusalStore, StoreError};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-shared-balance";
