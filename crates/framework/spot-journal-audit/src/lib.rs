//! The journal audit comparison and the per-tenant recovery report (WP-JRN-07.T1, ADR-061 s8 and
//! s9).
//!
//! The scheduled journal audit rebuilds the latest snapshot plus the journal into a scratch
//! database and compares it with the live one (ADR-061 s8). This crate is that comparison and the
//! report beside it, as pure code: it runs no rebuild, reads no clock, opens no file and touches
//! no store, so everything it needs — the records, the time, the tenant's targets — is an
//! argument.
//!
//! Both sides of the comparison arrive as **sorted iterators** of [`RecordSummary`] (by entity,
//! then id), so [`compare`] streams them in constant memory: it holds one record per side and the
//! sort key it saw last, and never collects a side.
//!
//! What it answers:
//!
//! * **Do the two databases hold the same records?** [`compare`] returns an [`AuditResult`] with
//!   per-entity counts on both sides, every kind of difference ([`DifferenceKind`]), the derived
//!   entities it skipped and listed, and the **first differing record in sort order** — the output
//!   ADR-061 s8 requires.
//! * **Is the financial fingerprint the same?** [`compare_fingerprints`] compares the named
//!   numeric sets of WP-MIG-06 exactly (Trial Balance, Stock Balance, AR and AP ageing): values
//!   are integer minor units or exact decimal strings, never floating point (ADR-12).
//! * **Was the recovery time objective met?** [`rto_check`] measures the rebuild against the
//!   tenant's own tier target, which is an argument (standard 4 h and premium 1 h are placeholders,
//!   ADR-061 open question 4).
//! * **Could this tenant be recovered now?** [`RecoveryReport`] lists the journal target and
//!   region, the durability policy, the last verified position and time, the oldest snapshot, the
//!   key backup status, the excluded entities and the blob cross-check, and answers [`Verdict::No`]
//!   with a reason for every input that says so.
//! * **What condition does a mismatch raise?** [`AuditOutcome::to_condition`] produces
//!   `JOURNAL_AUDIT_MISMATCH` (ADR-067 s2) as plain data for the conditions framework.
//!
//! Wiring the audit into the runtime — the scratch rebuild, the schedule, the Admin Center page
//! and the conditions framework — is a later task; this crate only compares and reports.

mod compare;
mod error;
mod fingerprint;
mod report;
mod result;
mod rto;
mod summary;

#[cfg(test)]
mod tests;

pub use compare::{compare, compare_all};
pub use error::AuditError;
pub use fingerprint::{
    Amount, Decimal, Fingerprint, FingerprintDifference, FingerprintDifferenceKind,
    FingerprintReport, FingerprintValue, ValueDifference, ValueDifferenceKind,
    compare_fingerprints,
};
pub use report::{
    BlobCrossCheck, Blocker, DurabilityPolicy, ExcludedEntity, ExclusionReason, JournalTarget,
    KeyBackupStatus, OldestSnapshot, Position, RecoveryReport, RecoveryReportBuilder, Verdict,
    VerifiedPosition,
};
pub use result::{
    AuditOutcome, AuditResult, Condition, ConditionParams, DifferenceKind, EntityCount,
    FirstDifference, JOURNAL_AUDIT_MISMATCH,
};
pub use rto::{RtoVerdict, rto_check};
pub use summary::{DerivedEntities, RecordSummary, content_digest};
