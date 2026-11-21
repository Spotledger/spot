//! Backup-set expiry, planned only.
//!
//! Backups are not documents, so the engine never runs them through a retention rule. What it does
//! is answer — before anything is touched — when each set expires under the rule that names it.
//! The backup subsystem (WP-PRD-04) consumes the [`BackupPlan`] and performs the expiry; this
//! crate only plans, which is what the task asks for.
//!
//! ```toml
//! [[rule]]
//! id = "backups-90d"
//! doctype = "backup_set"
//! trigger = "created"
//! period = "90d"
//! action = "delete"
//! ```
//!
//! A set that names a missing rule or whose date the calendar refuses is skipped with a reason,
//! never silently expired.

use crate::calendar::{Calendar, CivilDate};
use crate::error::DataPolicyError;
use crate::rule::RetentionRules;

/// One backup set to plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSet {
    /// The set's id (the backup reference).
    pub id: String,
    /// The retention rule that governs it.
    pub rule_id: String,
    /// When the set was made, in Unix milliseconds.
    pub created: u64,
}

/// When one backup set expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupExpiry {
    /// The set's id.
    pub id: String,
    /// The rule that set the expiry.
    pub rule_id: String,
    /// The expiry instant at midnight UTC, in Unix milliseconds.
    pub expires_at: u64,
}

impl BackupExpiry {
    /// Whether the set has expired at `now`.
    #[must_use]
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at <= now
    }
}

/// A set the planner could not date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSkip {
    /// The set's id.
    pub id: String,
    /// Why it was not planned.
    pub reason: String,
}

/// The result of planning backup expiry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackupPlan {
    /// Sets with an expiry.
    pub expiries: Vec<BackupExpiry>,
    /// Sets the planner could not date.
    pub skipped: Vec<BackupSkip>,
}

impl BackupPlan {
    /// The expiry of `id`, if planned.
    #[must_use]
    pub fn expiry(&self, id: &str) -> Option<&BackupExpiry> {
        self.expiries.iter().find(|expiry| expiry.id == id)
    }

    /// Whether `id` is expired at `now`; `None` when it was not planned.
    #[must_use]
    pub fn is_expired(&self, id: &str, now: u64) -> Option<bool> {
        self.expiry(id).map(|expiry| expiry.is_expired(now))
    }
}

/// Plans the expiry of every set. Pure: no clock, no store, no mutation.
#[must_use]
pub fn plan_backup_expiry(
    sets: &[BackupSet],
    rules: &RetentionRules,
    calendar: &dyn Calendar,
) -> BackupPlan {
    let mut plan = BackupPlan::default();
    for set in sets {
        let Some(rule) = rules.get(&set.rule_id) else {
            plan.skipped.push(BackupSkip {
                id: set.id.clone(),
                reason: format!("no rule {}", set.rule_id),
            });
            continue;
        };
        let dated = CivilDate::from_unix_millis(set.created)
            .and_then(|date| calendar.add(date, rule.period))
            .and_then(|date| date.to_unix_millis());
        match dated {
            Ok(expires_at) => plan.expiries.push(BackupExpiry {
                id: set.id.clone(),
                rule_id: set.rule_id.clone(),
                expires_at,
            }),
            Err(DataPolicyError::InvalidCalendar { detail, .. }) => {
                plan.skipped.push(BackupSkip {
                    id: set.id.clone(),
                    reason: detail,
                });
            }
            Err(error) => plan.skipped.push(BackupSkip {
                id: set.id.clone(),
                reason: error.to_string(),
            }),
        }
    }
    plan.expiries.sort_by(|a, b| a.id.cmp(&b.id));
    plan.skipped.sort_by(|a, b| a.id.cmp(&b.id));
    plan
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{BackupSet, plan_backup_expiry};
    use crate::calendar::{CivilDate, Gregorian};
    use crate::rule::RetentionRules;

    fn at(year: i32, month: u8, day: u8) -> u64 {
        CivilDate::new(year, month, day)
            .unwrap()
            .to_unix_millis()
            .unwrap()
    }

    #[test]
    fn a_backup_expires_under_its_rule() {
        let rules = RetentionRules::from_toml(
            "[[rule]]\nid = \"b90\"\ndoctype = \"backup_set\"\ntrigger = \"created\"\nperiod = \"90d\"\naction = \"delete\"\n",
        )
        .unwrap();
        let plan = plan_backup_expiry(
            &[BackupSet {
                id: "b1".to_owned(),
                rule_id: "b90".to_owned(),
                created: at(2026, 1, 1),
            }],
            &rules,
            &Gregorian,
        );
        assert_eq!(
            plan.expiry("b1").unwrap().expires_at,
            at(2026, 4, 1),
            "90 days after 1 January 2026"
        );
        assert_eq!(plan.is_expired("b1", at(2026, 3, 31)), Some(false));
        assert_eq!(plan.is_expired("b1", at(2026, 4, 1)), Some(true));
    }

    #[test]
    fn a_missing_rule_is_skipped_with_a_reason() {
        let rules = RetentionRules::empty();
        let plan = plan_backup_expiry(
            &[BackupSet {
                id: "b1".to_owned(),
                rule_id: "missing".to_owned(),
                created: at(2026, 1, 1),
            }],
            &rules,
            &Gregorian,
        );
        assert!(plan.expiries.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.contains("missing"));
    }
}
