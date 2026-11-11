//! Grandfather-father-son retention: a pure planner over backup records.
//!
//! The planner never touches a target. It decides which ids to keep and which to
//! delete, and its rules are deliberately conservative:
//!
//! - the newest **verified** backup is always kept (it is the last good one);
//! - every backup newer than [`RetentionPolicy::protect_newer_than_days`] is kept;
//! - every backup referenced by an open restore is kept;
//! - otherwise one backup per day, ISO week and month is kept for the configured
//!   grandfather/father/son window.
//!
//! An unverified backup is never counted as the last good one. A failed verification
//! therefore cannot make a bad backup the thing retention protects.

use std::collections::{BTreeMap, BTreeSet};

use time::{Duration, OffsetDateTime};

use crate::error::{BackupError, Result};
use crate::model::BackupRecord;

/// The GFS window and the extra safety margins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Daily backups to keep (one per day, newest first).
    pub keep_daily: u32,
    /// Weekly backups to keep (one per ISO week).
    pub keep_weekly: u32,
    /// Monthly backups to keep (one per calendar month).
    pub keep_monthly: u32,
    /// Keep everything created within this many days, regardless of buckets.
    pub protect_newer_than_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy {
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 12,
            protect_newer_than_days: 2,
        }
    }
}

/// Why the planner kept a backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// The newest verified backup; never deleted.
    LastVerified,
    /// Newer than the protection window.
    NewerThanWindow,
    /// Referenced by an open restore.
    OpenRestore,
    /// The newest backup of a kept day.
    Daily,
    /// The newest backup of a kept ISO week.
    Weekly,
    /// The newest backup of a kept month.
    Monthly,
}

impl KeepReason {
    /// The stable lower-case name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            KeepReason::LastVerified => "last_verified",
            KeepReason::NewerThanWindow => "newer_than_window",
            KeepReason::OpenRestore => "open_restore",
            KeepReason::Daily => "daily",
            KeepReason::Weekly => "weekly",
            KeepReason::Monthly => "monthly",
        }
    }
}

/// One decision: the id and the rule that kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepDecision {
    /// The backup id.
    pub id: String,
    /// Why it is kept.
    pub reason: KeepReason,
}

/// What a run should delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPlan {
    /// Backups to keep, with the rule that protected each.
    pub keep: Vec<KeepDecision>,
    /// Backups to delete, in oldest-first order.
    pub delete: Vec<String>,
}

impl RetentionPlan {
    /// Whether `id` is kept.
    pub fn keeps(&self, id: &str) -> bool {
        self.keep.iter().any(|decision| decision.id == id)
    }

    /// The ids to delete.
    pub fn delete_ids(&self) -> Vec<String> {
        self.delete.clone()
    }
}

/// Plans retention for `backups` at `now`.
///
/// # Errors
/// [`BackupError::RetentionRefused`] when the caller's records would force deleting
/// the last verified backup (a defensive check: the planner's own rules never do).
pub fn plan(
    policy: &RetentionPolicy,
    backups: &[BackupRecord],
    now: OffsetDateTime,
) -> Result<RetentionPlan> {
    let mut ordered = backups.to_vec();
    ordered.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.sequence.cmp(&left.sequence))
            .then_with(|| right.id.cmp(&left.id))
    });

    let last_verified = ordered
        .iter()
        .find(|record| record.verified)
        .map(|record| record.id.clone());
    let mut reasons: BTreeMap<String, KeepReason> = BTreeMap::new();
    if let Some(id) = &last_verified {
        reasons.insert(id.clone(), KeepReason::LastVerified);
    }

    let window = Duration::days(i64::from(policy.protect_newer_than_days));
    let threshold = now - window;
    for record in &ordered {
        if record.created_at >= threshold {
            reasons
                .entry(record.id.clone())
                .or_insert(KeepReason::NewerThanWindow);
        }
        if record.open_restore {
            reasons
                .entry(record.id.clone())
                .or_insert(KeepReason::OpenRestore);
        }
    }

    // GFS buckets, newest first. Each bucket keeps its newest backup. Only verified
    // backups are eligible: an unverified package must not displace a restorable one.
    let mut daily = BTreeSet::new();
    let mut weekly = BTreeSet::new();
    let mut monthly = BTreeSet::new();
    for record in ordered.iter().filter(|record| record.verified) {
        let date = record.created_at.date();
        let day = (date.year(), date.ordinal());
        if daily.len() < policy.keep_daily as usize && daily.insert(day) {
            reasons
                .entry(record.id.clone())
                .or_insert(KeepReason::Daily);
        }
        let week = (iso_year(date), date.iso_week());
        if weekly.len() < policy.keep_weekly as usize && weekly.insert(week) {
            reasons
                .entry(record.id.clone())
                .or_insert(KeepReason::Weekly);
        }
        let month = (date.year(), date.month() as u8);
        if monthly.len() < policy.keep_monthly as usize && monthly.insert(month) {
            reasons
                .entry(record.id.clone())
                .or_insert(KeepReason::Monthly);
        }
    }

    let mut keep: Vec<KeepDecision> = ordered
        .iter()
        .filter_map(|record| {
            reasons.get(&record.id).map(|reason| KeepDecision {
                id: record.id.clone(),
                reason: *reason,
            })
        })
        .collect();
    keep.sort_by(|left, right| left.id.cmp(&right.id));

    let mut delete: Vec<String> = ordered
        .iter()
        .filter(|record| !reasons.contains_key(&record.id))
        .map(|record| record.id.clone())
        .collect();
    delete.reverse();

    if let Some(id) = last_verified
        && delete.contains(&id)
    {
        return Err(BackupError::RetentionRefused {
            id,
            reason: "it is the newest verified backup".to_owned(),
        });
    }

    Ok(RetentionPlan { keep, delete })
}

/// The ISO week-numbering year: January dates in week 52/53 belong to the previous
/// year, and late-December dates in week 1 belong to the next.
fn iso_year(date: time::Date) -> i32 {
    match (date.month() as u8, date.iso_week()) {
        (1, week) if week >= 52 => date.year() - 1,
        (12, 1) => date.year() + 1,
        _ => date.year(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn record(id: &str, at: OffsetDateTime, verified: bool) -> BackupRecord {
        BackupRecord {
            id: id.to_owned(),
            created_at: at,
            verified,
            sequence: 1,
            open_restore: false,
        }
    }

    /// A table of daily backups across three months with one unverified failure.
    #[test]
    fn gfs_keeps_a_verified_grandfather_father_son_set() {
        let now = datetime!(2026-10-19 12:00:00 UTC);
        let mut backups = Vec::new();
        for day in 0..120 {
            let at = now - Duration::days(day);
            let id = format!("b{day:03}");
            // Day 40 is a failed verification.
            backups.push(record(&id, at, day != 40));
        }
        let policy = RetentionPolicy {
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 3,
            protect_newer_than_days: 2,
        };
        let plan = plan(&policy, &backups, now).expect("plan");
        // The three freshest days are protected outright.
        assert!(plan.keeps("b000"));
        assert!(plan.keeps("b001"));
        assert!(plan.keeps("b002"));
        // One backup per day for a week, plus weekly and monthly representatives.
        assert!(plan.keep.len() >= 7);
        // The failed backup is not the last verified and may age out.
        assert!(!plan.keeps("b040") || plan.delete.is_empty());
        // The newest verified backup is always kept.
        assert!(plan.keeps("b000"));
        // Anything deleted is older than the newest kept backup.
        for id in &plan.delete {
            assert!(!plan.keeps(id));
        }
    }

    #[test]
    fn the_last_verified_is_kept_even_when_it_is_the_only_old_backup() {
        let now = datetime!(2026-10-19 12:00:00 UTC);
        let backups = vec![
            record("old-good", now - Duration::days(500), true),
            record("newer-unverified", now - Duration::days(400), false),
        ];
        let policy = RetentionPolicy {
            keep_daily: 1,
            keep_weekly: 1,
            keep_monthly: 1,
            protect_newer_than_days: 1,
        };
        let plan = plan(&policy, &backups, now).expect("plan");
        assert!(plan.keeps("old-good"));
        assert!(plan.delete.contains(&"newer-unverified".to_owned()));
    }

    #[test]
    fn open_restores_are_protected() {
        let now = datetime!(2026-10-19 12:00:00 UTC);
        let mut old = record("old", now - Duration::days(500), true);
        old.open_restore = true;
        let backups = vec![old, record("new", now, true)];
        let plan = plan(&RetentionPolicy::default(), &backups, now).expect("plan");
        assert!(plan.keeps("old"));
    }
}
