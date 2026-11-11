//! The per-tenant recovery report, filled from real inputs (WP-JRN-07.T3, ADR-061 s8 and s9).
//!
//! `spot-journal-audit` holds the audit comparison and a report model built by hand. This crate
//! fills a report from the places the facts live, through traits: the control plane
//! ([`JournalFacts`]), the key service ([`KeyFacts`]), the owners' exclusions
//! ([`ExclusionRegistry`]) and the audit history ([`AuditHistory`]), plus the two inputs that
//! come from the object store, collected once by [`check_chain`] (the signed manifest and chain
//! verification) and [`read_snapshots`]. It is a library over traits: no network, no database, no
//! clock, no randomness.
//!
//! * [`build_report`] reads every input once and keeps the **facts** in a [`RecoveryReport`]. An
//!   input that cannot be read is [`Fact::Unreadable`], never a default.
//! * [`verdict`] decides from the report and [`Thresholds`] whether the tenant could be
//!   recovered: [`Verdict::Recoverable`], [`Verdict::NotRecoverable`] (some input says no) or
//!   [`Verdict::CannotConfirm`] (no input says no, but one is missing, unreadable or
//!   untrustworthy). It fails closed: absent facts are blockers, never a pass.
//! * [`assess`] does both and returns the pair a console shows. Its JSON form is stable and
//!   pinned by a golden test.
//!
//! The report reads metadata and counts only. It never reads a tenant's records, and no value of
//! one reaches the report or a reason; the parameters of a reason are counts, ages in days and
//! entity, owner and catalogue keys.
//!
//! # Reason keys
//!
//! A reason is a key with parameters, never prose. "Definite" keys say no; "unknown" keys say
//! the input could not be confirmed. The keys, for the page that explains them:
//!
//! | Key | Kind | Parameters |
//! | --- | --- | --- |
//! | `recovery.blocker.tenant_missing` | definite | |
//! | `recovery.blocker.journal_target_missing` | definite | |
//! | `recovery.blocker.region_missing` | definite | |
//! | `recovery.blocker.durability_policy_missing` | definite | |
//! | `recovery.blocker.local_policy_without_offsite_history` | definite | |
//! | `recovery.blocker.tail_lag_exceeded` | definite | `lag_millis`, `limit_millis` |
//! | `recovery.blocker.never_verified` | definite | |
//! | `recovery.blocker.verification_stale` | definite | `age_days`, `limit_days` |
//! | `recovery.blocker.last_audit_mismatch` | definite | `differing_records` |
//! | `recovery.blocker.no_snapshot` | definite | |
//! | `recovery.blocker.snapshot_too_old` | definite | `age_days`, `limit_days` |
//! | `recovery.blocker.journal_key_missing` | definite | |
//! | `recovery.blocker.key_backup_missing` | definite | |
//! | `recovery.blocker.key_backup_failed` | definite | |
//! | `recovery.blocker.key_backup_stale` | definite | `age_days`, `limit_days` |
//! | `recovery.blocker.blobs_missing` | definite | `count` |
//! | `recovery.blocker.excluded_entity_nameless` | definite | |
//! | `recovery.blocker.excluded_without_owner_reason` | definite | `entity` |
//! | `recovery.blocker.chain_tampering` | definite | `code`, `generation`, `first_seq`, `last_seq` |
//! | `recovery.blocker.input_unreadable` | unknown | `input` |
//! | `recovery.blocker.clock_skew` | unknown | `input` |
//! | `recovery.blocker.threshold_invalid` | unknown | `threshold` |
//! | `recovery.blocker.key_backup_unverified` | unknown | |
//! | `recovery.blocker.chain_check_incomplete` | unknown | |
//! | `recovery.blocker.blob_cross_check_not_run` | unknown | |

mod build;
mod chain;
mod error;
mod facts;
mod model;

#[cfg(test)]
mod tests;

pub use build::{RecoveryInputs, assess, build_report, verdict};
pub use chain::{ChainFacts, ChainFinding, ChainState, SnapshotFacts, check_chain, read_snapshots};
pub use error::FactUnreadable;
pub use facts::{
    AuditFailure, AuditHistory, Cadence, ExclusionEntry, ExclusionRegistry, FactResult,
    JournalFacts, KeyFacts,
};
pub use model::{
    Assessment, BlobSummary, Blocker, BlockerKind, BlockerReason, DAY_MILLIS, Fact, RecoveryReport,
    Thresholds, Verdict,
};
