//! The catalogue's notify steps and the obligation ladder's recipients agree at every level
//! (ADR-065 section 3): the contact at overdue, `sync_admin` at red, and nobody at amber.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_sync_acks::{OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE, OBLIGATION_RED};
use spot_sync_obligations::{
    Level, MandatoryEvent, Obligation, ObligationSettings, Recipient, ladder,
};

const H: i64 = 3_600_000;

fn recipient_action(who: Recipient) -> &'static str {
    match who {
        Recipient::NodeContact => "notify_contact",
        Recipient::SyncAdmin => "notify_role",
    }
}

fn obligation() -> Obligation {
    Obligation {
        event: MandatoryEvent {
            position: 5,
            published_at_millis: 0,
            effective_from_millis: None,
            record: "tax_rate/5".to_owned(),
        },
        held: false,
    }
}

/// The notify actions in the catalogue entry's response steps.
fn catalogue_notifies(id: &str) -> BTreeSet<String> {
    spot_sync_conditions::entry(id)
        .map(|e| e.def.response)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.action)
        .filter(|a| a.starts_with("notify_"))
        .collect()
}

/// The recipients the ladder produces for one rung alone, as catalogue action ids.
fn ladder_notifies(now: i64, last: Level, expect: Level) -> BTreeSet<String> {
    let out = ladder(
        "n1",
        &[obligation()],
        now,
        &ObligationSettings::default(),
        last,
    )
    .expect("default settings are valid");
    assert_eq!(out.level, expect);
    out.notifications
        .iter()
        .map(|n| recipient_action(n.who).to_owned())
        .collect()
}

#[test]
fn due_soon_has_no_notification_in_either_place() {
    // Half the 24 hour window is amber.
    assert!(ladder_notifies(13 * H, Level::Ok, Level::Amber).is_empty());
    assert!(catalogue_notifies(OBLIGATION_DUE_SOON).is_empty());
}

#[test]
fn the_overdue_rung_notifies_the_contact_only() {
    let from_ladder = ladder_notifies(24 * H + 1, Level::Ok, Level::Overdue);
    assert_eq!(from_ladder, catalogue_notifies(OBLIGATION_OVERDUE));
    assert_eq!(from_ladder, BTreeSet::from(["notify_contact".to_owned()]));
}

#[test]
fn the_red_rung_notifies_sync_admin_only() {
    // Past the 25 percent grace (6 hours) after the deadline; the contact was told at overdue.
    let from_ladder = ladder_notifies(31 * H, Level::Overdue, Level::Red);
    assert_eq!(from_ladder, catalogue_notifies(OBLIGATION_RED));
    assert_eq!(from_ladder, BTreeSet::from(["notify_role".to_owned()]));
}
