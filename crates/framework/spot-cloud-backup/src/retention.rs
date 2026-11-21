//! Retention: a pure planner over daily/weekly/monthly counts, and audited deletion.
//!
//! Planning and deleting are deliberately separate. [`plan_retention`] is a pure function
//! of the policy, the backup list, the instant and the ids an open restore references; it
//! returns what to keep, why, and what to delete. Only the plan mints
//! [`DeletionPermit`]s, and [`execute_retention`] is the one path that asks the service to
//! delete, so a delete is always a deliberate, audited consequence of a plan.
//!
//! The rules:
//!
//! - the newest **verified** backup is always kept (the restore point);
//! - every backup an open restore references is kept;
//! - the newest backup of each of the last *daily* days, *weekly* ISO weeks and *monthly*
//!   calendar months is kept, up to the policy's counts;
//! - a `pending` backup is kept, because an upload may still be in flight; garbage
//!   collection of abandoned uploads is a separate operation;
//! - an `unverified` backup fills no bucket (it is not a restore point) and is deleted
//!   unless an open restore references it;
//! - everything else is deleted, oldest first.

use std::collections::BTreeMap;

use spot_core::SiteId;
use time::OffsetDateTime;

use crate::crypto;
use crate::error::{CloudBackupError, Result};
use crate::manifest::{BackupManifest, BackupState};
use crate::service::{BackupService, DeletionPermit, TenantAuth};

/// How many daily, weekly and monthly backups to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Newest backup of each of the last `daily` days.
    pub daily: u32,
    /// Newest backup of each of the last `weekly` ISO weeks.
    pub weekly: u32,
    /// Newest backup of each of the last `monthly` calendar months.
    pub monthly: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        // A year of restore points: two weeks daily, two months weekly, a year monthly.
        Self {
            daily: 14,
            weekly: 8,
            monthly: 12,
        }
    }
}

impl RetentionPolicy {
    /// A policy with the given counts.
    ///
    /// # Errors
    /// [`CloudBackupError::Retention`] when every count is zero (nothing would ever be
    /// kept) or a count exceeds 3650.
    pub fn new(daily: u32, weekly: u32, monthly: u32) -> Result<Self> {
        if daily == 0 && weekly == 0 && monthly == 0 {
            return Err(CloudBackupError::Retention {
                reason: "a policy must keep at least one daily, weekly or monthly backup"
                    .to_owned(),
            });
        }
        if [daily, weekly, monthly].iter().any(|count| *count > 3650) {
            return Err(CloudBackupError::Retention {
                reason: "a retention count above 3650 is not supported".to_owned(),
            });
        }
        Ok(Self {
            daily,
            weekly,
            monthly,
        })
    }
}

/// The facts the planner needs about one backup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSummary {
    /// The backup id.
    pub backup_id: String,
    /// When it was taken, Unix seconds.
    pub created_at: i64,
    /// Its state.
    pub state: BackupState,
    /// Its ciphertext bytes.
    pub bytes: u64,
}

impl BackupSummary {
    /// The summary of a manifest.
    pub fn from_manifest(manifest: &BackupManifest) -> Self {
        Self {
            backup_id: manifest.backup_id.clone(),
            created_at: manifest.created_at,
            state: manifest.state.clone(),
            bytes: manifest.cipher_bytes(),
        }
    }
}

/// Why a backup is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepReason {
    /// The newest verified backup.
    LastVerified,
    /// An open restore references it.
    Referenced,
    /// The newest backup of a kept day.
    Daily,
    /// The newest backup of a kept ISO week.
    Weekly,
    /// The newest backup of a kept calendar month.
    Monthly,
    /// The upload has not finished; it is never retention's to delete.
    Pending,
}

impl KeepReason {
    /// A short stable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::LastVerified => "last_verified",
            Self::Referenced => "referenced",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Pending => "pending",
        }
    }
}

/// One kept backup and the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptBackup {
    /// The backup id.
    pub backup_id: String,
    /// Why it is kept.
    pub reason: KeepReason,
}

/// What a retention pass decided.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionPlan {
    /// A stable id for this plan (a digest of its inputs).
    pub plan_id: String,
    /// The backups to keep, newest first.
    pub keep: Vec<KeptBackup>,
    /// The backup ids to delete, oldest first.
    pub delete: Vec<String>,
    permits: Vec<DeletionPermit>,
}

impl RetentionPlan {
    /// The permits the plan mints, one per deletion.
    pub fn permits(&self) -> &[DeletionPermit] {
        &self.permits
    }

    /// Whether nothing is to be deleted.
    pub fn is_empty(&self) -> bool {
        self.delete.is_empty()
    }
}

/// Plans one tenant's retention.
///
/// # Errors
/// [`CloudBackupError::Retention`] for a duplicate backup id or an instant outside the
/// calendar range, and [`CloudBackupError::Store`] never (the planner has no I/O).
pub fn plan_retention(
    tenant: &SiteId,
    policy: &RetentionPolicy,
    backups: &[BackupSummary],
    now: i64,
    referenced: &[String],
) -> Result<RetentionPlan> {
    let mut ordered: Vec<&BackupSummary> = backups.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.backup_id.cmp(&right.backup_id))
    });
    for pair in ordered.windows(2) {
        if pair[0].backup_id == pair[1].backup_id {
            return Err(CloudBackupError::Retention {
                reason: format!("the backup id {} appears twice", pair[0].backup_id),
            });
        }
    }

    let mut reasons: BTreeMap<String, KeepReason> = BTreeMap::new();
    let mut keep = |backup: &BackupSummary, reason: KeepReason| {
        reasons.entry(backup.backup_id.clone()).or_insert(reason);
    };

    if let Some(verified) = ordered.iter().find(|backup| backup.state.is_verified()) {
        keep(verified, KeepReason::LastVerified);
    }
    for backup in &ordered {
        if backup.state == BackupState::Pending {
            keep(backup, KeepReason::Pending);
        }
        if referenced.iter().any(|id| id == &backup.backup_id) {
            keep(backup, KeepReason::Referenced);
        }
    }

    // Only verified backups are restore points, so only they fill the daily, weekly and
    // monthly buckets; an unverified backup is deleted unless something references it.
    let candidates: Vec<&BackupSummary> = ordered
        .iter()
        .copied()
        .filter(|backup| backup.state.is_verified())
        .collect();
    keep_newest_per_bucket(
        &candidates,
        policy.daily,
        |backup| Some(backup.created_at.div_euclid(86_400)),
        KeepReason::Daily,
        &mut keep,
    );
    keep_newest_per_bucket(
        &candidates,
        policy.weekly,
        |backup| {
            iso_week(backup.created_at).map(|week| i64::from(week.0) * 100 + i64::from(week.1))
        },
        KeepReason::Weekly,
        &mut keep,
    );
    keep_newest_per_bucket(
        &candidates,
        policy.monthly,
        |backup| month_key(backup.created_at),
        KeepReason::Monthly,
        &mut keep,
    );

    let mut kept = Vec::with_capacity(reasons.len());
    let mut delete = Vec::new();
    for backup in ordered.iter().rev() {
        match reasons.get(&backup.backup_id) {
            Some(reason) => kept.push(KeptBackup {
                backup_id: backup.backup_id.clone(),
                reason: *reason,
            }),
            None => delete.push(backup.backup_id.clone()),
        }
    }
    // `kept` was filled oldest-first; reports want newest-first.
    kept.reverse();

    let plan_id = plan_id(tenant, policy, &kept, &delete, now);
    let permits = delete
        .iter()
        .map(|backup_id| DeletionPermit {
            tenant: tenant.as_str().to_owned(),
            backup_id: backup_id.clone(),
            plan: plan_id.clone(),
        })
        .collect();
    Ok(RetentionPlan {
        plan_id,
        keep: kept,
        delete,
        permits,
    })
}

/// Keeps the newest backup of each of the first `count` buckets, newest bucket first.
fn keep_newest_per_bucket<F>(
    candidates: &[&BackupSummary],
    count: u32,
    bucket: F,
    reason: KeepReason,
    keep: &mut impl FnMut(&BackupSummary, KeepReason),
) where
    F: Fn(&BackupSummary) -> Option<i64>,
{
    if count == 0 {
        return;
    }
    let mut buckets: BTreeMap<i64, &BackupSummary> = BTreeMap::new();
    for backup in candidates {
        if let Some(key) = bucket(backup) {
            // `candidates` is newest first, so the first insert wins.
            buckets.entry(key).or_insert(backup);
        }
    }
    let mut keys: Vec<i64> = buckets.keys().copied().collect();
    keys.sort_unstable_by(|left, right| right.cmp(left));
    for key in keys.into_iter().take(usize::try_from(count).unwrap_or(0)) {
        if let Some(backup) = buckets.get(&key) {
            keep(backup, reason);
        }
    }
}

/// The ISO week of an instant.
fn iso_week(created_at: i64) -> Option<(i32, u8)> {
    OffsetDateTime::from_unix_timestamp(created_at)
        .ok()
        .map(|instant| instant.date())
        .map(|date| (date.year(), date.iso_week()))
}

/// The calendar month of an instant as `year * 100 + month`.
fn month_key(created_at: i64) -> Option<i64> {
    OffsetDateTime::from_unix_timestamp(created_at)
        .ok()
        .map(|instant| instant.date())
        .map(|date| i64::from(date.year()) * 100 + i64::from(u8::from(date.month())))
}

fn plan_id(
    tenant: &SiteId,
    policy: &RetentionPolicy,
    keep: &[KeptBackup],
    delete: &[String],
    now: i64,
) -> String {
    let mut input = format!(
        "{}\n{}/{}/{}\n{now}\n",
        tenant.as_str(),
        policy.daily,
        policy.weekly,
        policy.monthly
    );
    for kept in keep {
        input.push_str(&format!("keep:{}:{}\n", kept.backup_id, kept.reason.name()));
    }
    for id in delete {
        input.push_str(&format!("delete:{id}\n"));
    }
    crypto::sha256_hex(input.as_bytes())[..16].to_owned()
}

/// Executes a plan: every deletion goes through the service (which audits it first).
///
/// Returns how many backups were deleted.
///
/// # Errors
/// [`CloudBackupError::DeleteRefused`] when the service refuses a permit, and
/// [`CloudBackupError::Store`] for a transport failure. Deletions already done stay done;
/// the plan is safe to re-run because deleting an absent backup is refused rather than
/// repeated.
pub async fn execute_retention(
    service: &dyn BackupService,
    auth: &TenantAuth,
    plan: &RetentionPlan,
    actor: &str,
    at: i64,
) -> Result<u64> {
    let mut deleted = 0u64;
    for permit in plan.permits() {
        service.delete(auth, permit, actor, at).await?;
        deleted = deleted.saturating_add(1);
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup(id: &str, created_at: i64, state: BackupState) -> BackupSummary {
        BackupSummary {
            backup_id: id.to_owned(),
            created_at,
            state,
            bytes: 100,
        }
    }

    fn tenant() -> SiteId {
        SiteId::new("acme.example").expect("site")
    }

    #[test]
    fn the_newest_verified_backup_survives_an_empty_window() {
        let policy = RetentionPolicy::new(1, 1, 1).expect("policy");
        let now = 1_700_000_000;
        let backups = vec![
            backup(
                "bk-new",
                now,
                BackupState::Unverified { reason: "x".into() },
            ),
            backup("bk-old", now - 40 * 86_400, BackupState::Verified),
        ];
        let plan = plan_retention(&tenant(), &policy, &backups, now, &[]).expect("plan");
        assert!(plan.delete.contains(&"bk-new".to_owned()));
        assert!(
            plan.keep
                .iter()
                .any(|kept| kept.backup_id == "bk-old" && kept.reason == KeepReason::LastVerified)
        );
    }

    #[test]
    fn daily_weekly_and_monthly_buckets_keep_the_newest_of_each() {
        let policy = RetentionPolicy::new(2, 2, 2).expect("policy");
        let now = 1_700_000_000;
        let day = 86_400;
        let backups = vec![
            backup("d0a", now, BackupState::Verified),
            backup("d0b", now - 60, BackupState::Verified),
            backup("d1", now - day, BackupState::Verified),
            backup("d2", now - 2 * day, BackupState::Verified),
            backup("w1", now - 8 * day, BackupState::Verified),
            backup("m1", now - 40 * day, BackupState::Verified),
            backup("m2", now - 70 * day, BackupState::Verified),
        ];
        let plan = plan_retention(&tenant(), &policy, &backups, now, &[]).expect("plan");
        let kept: Vec<&str> = plan.keep.iter().map(|k| k.backup_id.as_str()).collect();
        // Newest verified plus the two newest days, weeks and months.
        assert!(kept.contains(&"d0a"));
        assert!(kept.contains(&"d1"));
        assert!(kept.contains(&"d2"));
        assert!(kept.contains(&"m1"));
        assert_eq!(
            plan.delete,
            vec!["m2".to_owned(), "w1".to_owned(), "d0b".to_owned()]
        );
        assert_eq!(kept.len(), 4);
    }

    #[test]
    fn referenced_and_pending_backups_are_never_deleted() {
        let policy = RetentionPolicy::new(1, 1, 1).expect("policy");
        let now = 1_700_000_000;
        let backups = vec![
            backup("pending", now - 900 * 86_400, BackupState::Pending),
            backup("referenced", now - 800 * 86_400, BackupState::Verified),
            backup(
                "old",
                now - 700 * 86_400,
                BackupState::Unverified {
                    reason: "corrupt chunk".into(),
                },
            ),
        ];
        let plan = plan_retention(
            &tenant(),
            &policy,
            &backups,
            now,
            &["referenced".to_owned()],
        )
        .expect("plan");
        let kept: Vec<&str> = plan.keep.iter().map(|k| k.backup_id.as_str()).collect();
        assert!(kept.contains(&"pending"));
        assert!(kept.contains(&"referenced"));
        assert!(plan.delete.contains(&"old".to_owned()));
    }

    #[test]
    fn permits_carry_the_plan_and_tenant() {
        let policy = RetentionPolicy::new(1, 0, 0).expect("policy");
        let now = 1_700_000_000;
        let backups = vec![
            backup("keep", now, BackupState::Verified),
            backup("drop", now - 10 * 86_400, BackupState::Verified),
        ];
        let plan = plan_retention(&tenant(), &policy, &backups, now, &[]).expect("plan");
        assert_eq!(plan.delete, vec!["drop".to_owned()]);
        assert_eq!(plan.permits().len(), 1);
        assert_eq!(plan.permits()[0].backup_id(), "drop");
        assert_eq!(plan.permits()[0].plan(), plan.plan_id);
        assert!(!plan.plan_id.is_empty());
    }

    #[test]
    fn duplicate_ids_and_empty_policies_are_refused() {
        let now = 1_700_000_000;
        let backups = vec![
            backup("same", now, BackupState::Verified),
            backup("same", now - 10, BackupState::Verified),
        ];
        assert!(RetentionPolicy::new(0, 0, 0).is_err());
        assert!(RetentionPolicy::new(0, 0, 3651).is_err());
        let policy = RetentionPolicy::new(1, 1, 1).expect("policy");
        assert!(plan_retention(&tenant(), &policy, &backups, now, &[]).is_err());
    }
}
