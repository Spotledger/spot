//! Building the report from the inputs and deciding the verdict from the report.

use spot_journal_audit::{BlobCrossCheck, DurabilityPolicy, KeyBackupStatus};

use crate::chain::{ChainFacts, ChainState, SnapshotFacts};
use crate::facts::{AuditHistory, Cadence, ExclusionRegistry, JournalFacts, KeyFacts};
use crate::model::{
    Assessment, BlobSummary, Blocker, DAY_MILLIS, Fact, RecoveryReport, Thresholds, Verdict,
};

/// Everything the report is built from. The store-backed inputs arrive already collected
/// ([`crate::check_chain`], [`crate::read_snapshots`]); the rest are read through the traits.
pub struct RecoveryInputs<'a> {
    /// The tenant. The report is about this tenant only.
    pub tenant: &'a str,
    /// The control plane's facts.
    pub journal: &'a dyn JournalFacts,
    /// The key service's facts.
    pub keys: &'a dyn KeyFacts,
    /// The exclusion registry.
    pub exclusions: &'a dyn ExclusionRegistry,
    /// The audit history.
    pub history: &'a dyn AuditHistory,
    /// The chain and manifest check.
    pub chain: ChainFacts,
    /// The snapshot listing.
    pub snapshots: SnapshotFacts,
    /// The blob cross-check; `None` when it did not run.
    pub blobs: Option<BlobCrossCheck>,
}

/// Builds the report as of `now_millis` (supplied by the caller; the library reads no clock).
///
/// It reads each input once. An input that cannot be read becomes [`Fact::Unreadable`]; nothing
/// is defaulted to a passing value.
#[must_use]
pub fn build_report(inputs: &RecoveryInputs<'_>, now_millis: i64) -> RecoveryReport {
    let tenant = inputs.tenant.to_owned();
    // A chain check about another tenant says nothing about this one.
    let chain = if inputs.chain.tenant == inputs.tenant {
        inputs.chain.clone()
    } else {
        ChainFacts::unreadable(inputs.tenant)
    };
    let mut excluded = Fact::from_result(inputs.exclusions.exclusions());
    if let Fact::Known(entries) = &mut excluded {
        entries.sort();
        entries.dedup();
    }
    RecoveryReport {
        tenant,
        generated_at_millis: now_millis,
        journal: Fact::from_result(inputs.journal.journal_target()),
        durability_policy: Fact::from_result(inputs.journal.durability_policy()),
        offsite_history: Fact::from_result(inputs.journal.has_offsite_history()),
        tail_lag_millis: Fact::from_result(inputs.journal.tail_lag_millis()),
        audit_cadence: Fact::from_result(inputs.journal.audit_cadence()),
        last_verified: Fact::from_result(inputs.history.last_verified()),
        last_failure: Fact::from_result(inputs.history.last_failure()),
        snapshots_readable: inputs.snapshots.readable,
        snapshot_count: inputs.snapshots.count,
        snapshots_verified: inputs.snapshots.verified,
        oldest_snapshot: inputs.snapshots.oldest,
        journal_key_present: Fact::from_result(inputs.keys.journal_key_present()),
        key_backup: Fact::from_result(inputs.keys.key_backup()),
        excluded_entities: excluded,
        chain,
        blob_cross_check: inputs.blobs.as_ref().map(|b| BlobSummary {
            checked: b.checked,
            missing: b.missing.len() as u64,
        }),
    }
}

fn unreadable(input: &str) -> Blocker {
    Blocker::InputUnreadable {
        input: input.to_owned(),
    }
}

fn days(millis: i64) -> i64 {
    millis / DAY_MILLIS
}

/// Judges an age against a limit. `Ok(Some(..))` is too old, `Ok(None)` is fine, and `Err` is a
/// timestamp in the future or a limit that is not positive. A limit is the age that is still
/// fine: the boundary passes.
fn judge_age(
    at_millis: i64,
    now_millis: i64,
    limit_millis: i64,
    input: &str,
    threshold: &str,
) -> Result<Option<(i64, i64)>, Blocker> {
    if limit_millis <= 0 {
        return Err(Blocker::ThresholdInvalid {
            threshold: threshold.to_owned(),
        });
    }
    if at_millis > now_millis {
        return Err(Blocker::ClockSkew {
            input: input.to_owned(),
        });
    }
    let age = now_millis.saturating_sub(at_millis);
    Ok((age > limit_millis).then(|| (days(age), days(limit_millis))))
}

/// Decides whether the tenant could be recovered, from the report alone.
///
/// Every reason is named, in the stable order of [`Blocker`]. A blocker that says no makes the
/// verdict `NotRecoverable`; if none does but one cannot be confirmed (an unreadable or missing
/// input), the verdict is `CannotConfirm`. `Recoverable` needs every input read and clean.
#[must_use]
pub fn verdict(report: &RecoveryReport, thresholds: &Thresholds) -> Verdict {
    let now = report.generated_at_millis;
    let mut blockers: Vec<Blocker> = Vec::new();

    if report.tenant.trim().is_empty() {
        blockers.push(Blocker::TenantMissing);
    }

    match &report.journal {
        Fact::Unreadable => blockers.push(unreadable("journal_target")),
        Fact::Known(None) => blockers.push(Blocker::JournalTargetMissing),
        Fact::Known(Some(target)) => {
            if target.target.trim().is_empty() {
                blockers.push(Blocker::JournalTargetMissing);
            }
            if target.region.trim().is_empty() {
                blockers.push(Blocker::RegionMissing);
            }
        }
    }

    match &report.durability_policy {
        Fact::Unreadable => blockers.push(unreadable("durability_policy")),
        Fact::Known(None) => blockers.push(Blocker::DurabilityPolicyMissing),
        Fact::Known(Some(DurabilityPolicy::Local)) => match report.offsite_history {
            Fact::Known(false) => blockers.push(Blocker::LocalPolicyWithoutOffsiteHistory),
            Fact::Known(true) => {}
            Fact::Unreadable => blockers.push(unreadable("offsite_history")),
        },
        Fact::Known(Some(_)) => {}
    }

    if let Some(limit) = thresholds.tail_lag_max_millis {
        match report.tail_lag_millis {
            Fact::Known(Some(lag)) if limit > 0 => {
                if lag > limit {
                    blockers.push(Blocker::TailLagExceeded {
                        lag_millis: lag,
                        limit_millis: limit,
                    });
                }
            }
            Fact::Known(Some(_)) => blockers.push(Blocker::ThresholdInvalid {
                threshold: "tail_lag_max".to_owned(),
            }),
            Fact::Known(None) | Fact::Unreadable => blockers.push(unreadable("tail_lag")),
        }
    }

    verification_blockers(report, thresholds, now, &mut blockers);

    if report.snapshots_readable {
        match &report.oldest_snapshot {
            None => blockers.push(Blocker::NoSnapshot),
            Some(oldest) => {
                match judge_age(
                    oldest.taken_at_millis,
                    now,
                    thresholds.snapshot_max_age_millis,
                    "oldest_snapshot",
                    "snapshot_max_age",
                ) {
                    Ok(Some((age_days, limit_days))) => blockers.push(Blocker::SnapshotTooOld {
                        age_days,
                        limit_days,
                    }),
                    Ok(None) => {}
                    Err(blocker) => blockers.push(blocker),
                }
            }
        }
    } else {
        blockers.push(unreadable("snapshots"));
    }

    match report.journal_key_present {
        Fact::Known(true) => {}
        Fact::Known(false) => blockers.push(Blocker::JournalKeyMissing),
        Fact::Unreadable => blockers.push(unreadable("journal_key")),
    }
    match &report.key_backup {
        Fact::Unreadable => blockers.push(unreadable("key_backup")),
        Fact::Known(KeyBackupStatus::Never) => blockers.push(Blocker::KeyBackupMissing),
        Fact::Known(KeyBackupStatus::Failed) => blockers.push(Blocker::KeyBackupFailed),
        Fact::Known(KeyBackupStatus::Unverified) => blockers.push(Blocker::KeyBackupUnverified),
        Fact::Known(KeyBackupStatus::Confirmed { verified_at_millis }) => {
            match judge_age(
                *verified_at_millis,
                now,
                thresholds.key_backup_max_age_millis,
                "key_backup",
                "key_backup_max_age",
            ) {
                Ok(Some((age_days, limit_days))) => blockers.push(Blocker::KeyBackupStale {
                    age_days,
                    limit_days,
                }),
                Ok(None) => {}
                Err(blocker) => blockers.push(blocker),
            }
        }
    }

    match &report.excluded_entities {
        Fact::Unreadable => blockers.push(unreadable("exclusions")),
        Fact::Known(entries) => {
            for entry in entries {
                if entry.entity.trim().is_empty() {
                    blockers.push(Blocker::ExcludedEntityNameless);
                } else if !entry.has_owner_reason() {
                    blockers.push(Blocker::ExcludedWithoutOwnerReason {
                        entity: entry.entity.clone(),
                    });
                }
            }
        }
    }

    match report.chain.state {
        ChainState::Unreadable => blockers.push(unreadable("chain")),
        ChainState::Partial => blockers.push(Blocker::ChainCheckIncomplete),
        ChainState::Complete => {}
    }
    for finding in report.chain.findings.iter().filter(|f| f.tampering) {
        blockers.push(Blocker::ChainTampering {
            code: finding.code.clone(),
            generation: finding.generation,
            first_seq: finding.first_seq,
            last_seq: finding.last_seq,
        });
    }

    match &report.blob_cross_check {
        None => blockers.push(Blocker::BlobCrossCheckNotRun),
        Some(blobs) if blobs.missing > 0 => blockers.push(Blocker::BlobsMissing {
            count: blobs.missing,
        }),
        Some(_) => {}
    }

    blockers.sort();
    blockers.dedup();
    if blockers.is_empty() {
        return Verdict::Recoverable;
    }
    let definite = blockers
        .iter()
        .any(|b| b.kind() == crate::model::BlockerKind::Definite);
    let reasons: Vec<_> = blockers.iter().map(Blocker::reason).collect();
    if definite {
        Verdict::NotRecoverable { blockers: reasons }
    } else {
        Verdict::CannotConfirm { blockers: reasons }
    }
}

/// The blockers of the verification history: never verified, stale, in the future, and a
/// mismatch no clean verification has followed.
fn verification_blockers(
    report: &RecoveryReport,
    thresholds: &Thresholds,
    now: i64,
    blockers: &mut Vec<Blocker>,
) {
    match &report.last_verified {
        Fact::Unreadable => blockers.push(unreadable("last_verified")),
        Fact::Known(None) => blockers.push(Blocker::NeverVerified),
        Fact::Known(Some(verified)) => match report.audit_cadence {
            Fact::Unreadable => blockers.push(unreadable("audit_cadence")),
            Fact::Known(cadence) => {
                let (limit, name) = match cadence {
                    Cadence::Daily => (
                        thresholds.verification_max_age_daily_millis,
                        "verification_max_age_daily",
                    ),
                    Cadence::Weekly => (
                        thresholds.verification_max_age_weekly_millis,
                        "verification_max_age_weekly",
                    ),
                };
                match judge_age(verified.at_millis, now, limit, "last_verified", name) {
                    Ok(Some((age_days, limit_days))) => {
                        blockers.push(Blocker::VerificationStale {
                            age_days,
                            limit_days,
                        });
                    }
                    Ok(None) => {}
                    Err(blocker) => blockers.push(blocker),
                }
            }
        },
    }
    match (&report.last_failure, &report.last_verified) {
        (Fact::Unreadable, _) => blockers.push(unreadable("last_failure")),
        (Fact::Known(None), _) => {}
        (Fact::Known(Some(failure)), verified) => {
            // A clean verification stamped after the failure supersedes it. When the history of
            // verifications cannot be read, the failure stands.
            let superseded = matches!(
                verified,
                Fact::Known(Some(v)) if v.at_millis > failure.at_millis
            );
            if !superseded {
                blockers.push(Blocker::LastAuditMismatch {
                    differing_records: failure.differing_records,
                });
            }
        }
    }
}

/// Builds the report and decides the verdict in one call.
#[must_use]
pub fn assess(inputs: &RecoveryInputs<'_>, now_millis: i64, thresholds: &Thresholds) -> Assessment {
    let report = build_report(inputs, now_millis);
    let verdict = verdict(&report, thresholds);
    Assessment { report, verdict }
}
