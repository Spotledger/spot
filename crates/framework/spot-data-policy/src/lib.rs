#![forbid(unsafe_code)]
//! Data residency and retention per tenant (WP-SEC-05.T1, ADR-058 rule 1, ADR-071).
//!
//! This crate answers the two questions SEC-05 promises: **tenant data stays in its declared
//! region**, and **retention is enforced**. It is a decision and execution core; it owns no
//! database, reads no clock and holds no secrets.
//!
//! # Residency
//!
//! [`ResidencyPolicy`] is data (TOML) with `allowed_regions`, `forbidden_regions` and
//! `backup_regions`. Four pure checks return a typed [`Refusal`] naming the policy clause:
//!
//! - [`check_placement`] — a new tenant, database, schema, replica, analytics mirror, search
//!   store or rehearsal clone;
//! - [`check_move`] — a tenant move, which additionally needs the owner's approval across
//!   regions;
//! - [`check_backup_target`] — a backup, snapshot or archive target;
//! - [`check_egress_region`] — any data leaving for an endpoint or region, including the
//!   `spot-llm-router` allowed-endpoint setting.
//!
//! [`RegionMap`] is the data table that says which region an endpoint, cluster or object store is
//! in; an unknown resource is refused, never assumed.
//!
//! # Retention
//!
//! 1. [`RetentionRules::from_toml`] loads rules (DocType, trigger, calendar-aware period, action,
//!    exceptions); [`Holds::from_toml`] loads legal holds and [`StatutoryFloor::from_toml`] the
//!    statutory floors (empty by default — no jurisdiction's periods are hard-coded here).
//! 2. [`plan_run`] reads a [`RetentionStore`] and returns a [`RetentionPlan`]: every item it would
//!    action with the reason it is due, every record it leaves alone with the reason, and counts
//!    per rule. It changes nothing.
//! 3. [`execute`] applies the plan in batches, dry-run by default, with a run identity, per-item
//!    outcomes, checkpoints and resume. Each applied item is one store transaction carrying the
//!    effect, outcome, audit record and — for a deletion — a [`DeletionProof`].
//!
//! Anonymisation replaces the fields the DocType metadata classifies as personal (`pii` or
//! `sensitive`) and not ledger-relevant, so totals and posted amounts survive. A legal hold
//! suspends every action; a statutory floor always wins over a shorter rule; a record another
//! retained record still links to is skipped with the referrers named.
//!
//! # Wiring
//!
//! `spot-tenancy`, `spot-tenant-move`, `spot-cloud-backup` and `spot-llm-router` call these pure
//! checks; their crates are outside this task's owned paths, so the wiring is a follow-up rather
//! than a change here (see the task report). [`plan_backup_expiry`] is the backup subsystem's
//! planner-only seam.
//!
//! # Time
//!
//! Every instant is Unix milliseconds supplied by the caller, and every period is calendar-aware
//! through the injected [`Calendar`] ([`Gregorian`] ships here). No function reads the wall clock.

pub mod backup;
pub mod calendar;
pub mod error;
pub mod execute;
pub mod hold;
pub mod plan;
pub mod proof;
pub mod record;
pub mod region;
pub mod rule;
pub mod statutory;
pub mod store;

pub use backup::{BackupExpiry, BackupPlan, BackupSet, BackupSkip, plan_backup_expiry};
pub use calendar::{Calendar, CivilDate, Gregorian, Period, PeriodUnit};
pub use error::{DataPolicyError, codes};
pub use execute::{DEFAULT_KIND, ExecutionOptions, ExecutionReport, Resume, execute};
pub use hold::{Hold, HoldScope, Holds};
pub use plan::{PlannedAction, PlannedSkip, PolicySet, RetentionPlan, RuleCount, plan_run};
pub use proof::DeletionProof;
pub use record::{Classification, FieldClassMap, FieldSpec, FieldValue, Record};
pub use region::{
    Clause, Refusal, Region, RegionChangeApproval, RegionMap, ResidencyPolicy, ResourceKind,
    ResourceRegion, check_backup_target, check_egress_region, check_move, check_placement,
};
pub use rule::{Action, Exceptions, RetentionRule, RetentionRules, Trigger};
pub use statutory::{FloorEntry, FloorLookup, StatutoryFloor};
pub use store::{
    AppliedEffect, ApplyItem, Checkpoint, Effect, ItemOutcome, MemoryStore, OutcomeStatus,
    RetentionStore, SkipReason,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-data-policy";
