//! Backup-set expiry is planned from the retention rule and performs no work itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_data_policy::{BackupSet, Gregorian, RetentionRules, plan_backup_expiry};

#[test]
fn backup_expiry_follows_the_rule_and_is_only_a_plan() {
    let rules = RetentionRules::from_toml(
        "[[rule]]\nid = \"backups-90d\"\ndoctype = \"backup_set\"\ntrigger = \"created\"\nperiod = \"90d\"\naction = \"delete\"\n\
         [[rule]]\nid = \"monthly-1y\"\ndoctype = \"monthly_backup_set\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n",
    )
    .unwrap();
    let plan = plan_backup_expiry(
        &[
            BackupSet {
                id: "daily-1".to_owned(),
                rule_id: "backups-90d".to_owned(),
                created: common::at(2026, 1, 1),
            },
            BackupSet {
                id: "monthly-1".to_owned(),
                rule_id: "monthly-1y".to_owned(),
                created: common::at(2026, 1, 1),
            },
            BackupSet {
                id: "orphan".to_owned(),
                rule_id: "no-such-rule".to_owned(),
                created: common::at(2026, 1, 1),
            },
        ],
        &rules,
        &Gregorian,
    );
    assert_eq!(
        plan.expiry("daily-1").unwrap().expires_at,
        common::at(2026, 4, 1)
    );
    assert_eq!(
        plan.expiry("monthly-1").unwrap().expires_at,
        common::at(2027, 1, 1)
    );
    assert_eq!(
        plan.is_expired("daily-1", common::at(2026, 3, 31)),
        Some(false)
    );
    assert_eq!(
        plan.is_expired("daily-1", common::at(2026, 4, 2)),
        Some(true)
    );
    assert_eq!(plan.expiry("orphan"), None);
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].id, "orphan");
}
