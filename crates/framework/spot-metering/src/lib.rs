//! Usage metering: what a tenant actually consumed, and what their licence allows.
//!
//! The licence ([`spot_license`]) says what a tenant *may* use; the LLM router's
//! ledger counts AI tokens in its own store; nothing here owns a storage engine, a
//! clock or the network. This crate is the meter and the judge between those two:
//!
//! - [`MeterEvent`] is one observation (a sign-in, a posted document, a storage-level reading,
//!   a settled AI request) carried under an idempotency key. Recording the same id
//!   twice counts once — the guarantee a retried producer needs.
//! - [`MeterSink`] is the synchronous, non-blocking front door producers call;
//!   [`MeterStore`] is the object-safe async persistence contract, with an
//!   in-memory implementation ([`MemoryMeterStore`]) and a contract suite
//!   ([`contract`]::`run_store_contract`) every backend must pass.
//! - [`PeriodSpec`] buckets instants into monthly periods in a tenant's zone
//!   (ADR-077: instants are UTC, periods are business dates in the business zone).
//! - [`QuotaPolicy`] and [`evaluate_quota`] turn meter totals plus
//!   [`spot_license::Entitlements`] into per-meter [`QuotaDecision`]s; the action
//!   classes a [`QuotaDecision::Blocked`] affects are deliberate: creating new users
//!   or new metered documents stops, while reading, exporting, signing in and
//!   reversals never do.
//! - [`QuotaGuard::allow`] is the hot path: it reads an `Arc`-swapped snapshot, no
//!   lock and no store call.
//! - [`reconcile`] compares the store's totals against an independently computed
//!   truth; the acceptance property is that any permutation, duplication and
//!   drop-retry of a batch lands on exactly the ground truth.
//!
//! # The store's two boundaries
//!
//! A store is **bound to one tenant** at construction: [`MeterStore::tenant`] is all it
//! writes, an event carrying another tenant is refused with
//! [`MeteringError::TenantMismatch`] before any write, and a read naming another tenant
//! is refused the same way. The tenant is a property of the store a caller was handed,
//! never of the payload, so a producer cannot name another tenant and land in its
//! totals.
//!
//! A store **bounds one call**: [`MAX_BATCH_EVENTS`] events per `add_events`, or the
//! whole batch is refused with [`MeteringError::BatchTooLarge`] before any write.
//!
//! # Compaction, the tombstone horizon, and replay
//!
//! Idempotency needs a tombstone: the remembered id of every event ever counted.
//! Keeping them all forever makes the store grow with the platform's whole history
//!, so [`RetentionPolicy`] bounds them. Compaction removes the tombstones of
//! periods that are **both** final (the caller's `before` cut says their totals may no
//! longer change) **and** older than `tombstone_horizon_periods` periods before that
//! cut. The same cut becomes the store's late-arrival floor, so from then on:
//!
//! - a replay **inside** the horizon finds its tombstone and is a no-op;
//! - anything **beyond** the horizon — a replay whose tombstone was purged, or a
//!   first-time event that late — is refused as
//!   [`MeteringError::LateBeyondHorizon`] and never counted.
//!
//! The two sets are the same set by construction, so a purged id cannot be counted
//! twice. [`retention`] states the invariant in full and how an operator sizes the
//! horizon; the contract suite's `TombstonesExpireBeyondTheHorizon`,
//! `ReplayInsideTheHorizonIsANoOp` and `LateArrivalBeyondTheHorizonIsRefused` cases
//! prove it against every implementation.
//!
//! # Hard rules
//!
//! No clock and no environment reads: time is an argument everywhere (the crate has
//! no way to be wrong about "now" silently). No floats: fractions are integer basis
//! points (workspace lint). No document contents, user names or prompts are stored
//! or logged: only id digests as set members and counts.
//!
//! An expired licence is *not* this crate's decision: it reports usage and defers to
//! [`spot_license`]; a licence in grace behaves as the licence's own state says.

pub mod adapters;
pub mod contract;
pub mod decision;
pub mod error;
pub mod event;
pub mod guard;
pub mod meter;
pub mod period;
pub mod policy;
pub mod reconcile;
pub mod retention;
pub mod sink;
pub mod store;
pub mod totals;

pub use adapters::{AiTokensFromLedger, UsersFromDirectory, active_user_events, token_sum};
pub use contract::{StoreContractCase, StoreFactory};
pub use decision::{
    ActionDecision, QuotaAction, QuotaDecision, QuotaReport, evaluate as evaluate_quota,
    grace_until_for, over_soft_limit, verdict_for,
};
pub use error::MeteringError;
pub use event::MeterEvent;
pub use guard::{QuotaGuard, QuotaSnapshot, RefreshContext, Staleness};
pub use meter::{MemberDigest, Meter, MeterKind};
pub use period::{PeriodBasis, PeriodKey, PeriodResolver, PeriodSpec, TenantPeriods, period_of};
pub use policy::{
    MeterLimit, QUOTA_SOFT_TOLERANCE_BPS, QUOTA_WARN_FRACTION_BPS, QuotaMode, QuotaPolicy,
    QuotaRule,
};
pub use reconcile::{ReconcileReport, ReconcileRow, TallyTruth, TruthSource, reconcile};
pub use retention::RetentionPolicy;
pub use sink::{BoundedSink, MeterSink, MeterTelemetry, SinkDropReason, SinkReceipt};
pub use store::BoxFuture;
pub use store::{
    CompactionReport, MAX_BATCH_EVENTS, MemoryMeterStore, MeterStore, compact_up_to, totals_for,
};
pub use totals::{PeriodTotals, TenantPeriodUsage};

/// The crate's semver-excluded version, for the error catalogue's `since` rows.
pub const VERSION: &str = "0.1.0";
