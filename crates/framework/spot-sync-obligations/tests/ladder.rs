//! Ladder transitions at exact boundaries, and idempotence.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_sync_obligations::{
    Level, MandatoryEvent, Notify, Obligation, ObligationSettings, Recipient, ladder, level_of,
};

const H: i64 = 3_600_000;

fn ob(position: u64, published: i64, effective: Option<i64>) -> Obligation {
    Obligation {
        event: MandatoryEvent {
            position,
            published_at_millis: published,
            effective_from_millis: effective,
            record: "r".into(),
        },
        held: false,
    }
}

#[test]
fn level_boundaries_to_the_millisecond() {
    let s = ObligationSettings::default();
    let o = ob(1, 0, None); // window 24h: amber 12h, overdue 24h, grace max(6h, 2h) = 6h, red 30h
    assert_eq!(level_of(&o, 12 * H - 1, &s), Level::Ok);
    assert_eq!(level_of(&o, 12 * H, &s), Level::Amber);
    assert_eq!(level_of(&o, 24 * H - 1, &s), Level::Amber);
    assert_eq!(level_of(&o, 24 * H, &s), Level::Overdue);
    assert_eq!(level_of(&o, 30 * H - 1, &s), Level::Overdue);
    assert_eq!(level_of(&o, 30 * H, &s), Level::Red);
}

#[test]
fn grace_minimum_applies_for_short_windows() {
    let s = ObligationSettings {
        delivery_window_millis: 4 * H,
        ..ObligationSettings::default()
    };
    assert_eq!(s.grace_millis(), 2 * H); // 25% of 4h is 1h, minimum 2h wins
    let o = ob(1, 0, None);
    assert_eq!(level_of(&o, 6 * H - 1, &s), Level::Overdue);
    assert_eq!(level_of(&o, 6 * H, &s), Level::Red);
}

#[test]
fn near_effective_date_is_red_regardless_of_the_ladder() {
    let s = ObligationSettings::default();
    let o = ob(1, 0, Some(100 * H)); // red from 100h - 24h = 76h
    assert_eq!(level_of(&o, 76 * H - 1, &s), Level::Red); // 76h-1 is past the 24h window's grace anyway
    let fresh = ob(2, 70 * H, Some(100 * H)); // due 88h; window ok at 70h
    assert_eq!(level_of(&fresh, 76 * H - 1, &s), Level::Ok);
    assert_eq!(level_of(&fresh, 76 * H, &s), Level::Red);
}

#[test]
fn amber_is_half_of_a_shortened_window() {
    // No near-date red here, so the shortened window itself is visible.
    let s = ObligationSettings {
        effective_red_lead_millis: 0,
        ..ObligationSettings::default()
    };
    let o = ob(1, 0, Some(30 * H)); // due 18h, amber 9h
    assert_eq!(level_of(&o, 9 * H - 1, &s), Level::Ok);
    assert_eq!(level_of(&o, 9 * H, &s), Level::Amber);
}

#[test]
fn transitions_notify_once() {
    let s = ObligationSettings::default();
    let obs = vec![ob(1, 0, None), ob(2, 20 * H, None)];

    let a = ladder("b1", &obs, 13 * H, &s, Level::Ok).unwrap();
    assert_eq!(a.level, Level::Amber);
    assert!(a.notifications.is_empty());

    let o = ladder("b1", &obs, 24 * H, &s, a.level).unwrap();
    assert_eq!(o.level, Level::Overdue);
    assert_eq!(
        o.notifications,
        vec![Notify {
            who: Recipient::NodeContact,
            level: Level::Overdue,
            node: "b1".into(),
            obligations: vec![1]
        }]
    );
    // Same level again: nothing more.
    assert!(
        ladder("b1", &obs, 25 * H, &s, o.level)
            .unwrap()
            .notifications
            .is_empty()
    );

    let r = ladder("b1", &obs, 30 * H, &s, o.level).unwrap();
    assert_eq!(r.level, Level::Red);
    assert_eq!(
        r.notifications,
        vec![Notify {
            who: Recipient::SyncAdmin,
            level: Level::Red,
            node: "b1".into(),
            obligations: vec![1]
        }]
    );
    assert!(
        ladder("b1", &obs, 31 * H, &s, r.level)
            .unwrap()
            .notifications
            .is_empty()
    );
}

#[test]
fn jumping_straight_to_red_notifies_both() {
    let s = ObligationSettings::default();
    let r = ladder("b1", &[ob(1, 0, None)], 40 * H, &s, Level::Ok).unwrap();
    let who: Vec<Recipient> = r.notifications.iter().map(|n| n.who).collect();
    assert_eq!(who, vec![Recipient::NodeContact, Recipient::SyncAdmin]);
}

#[test]
fn falling_level_does_not_notify_and_empty_is_ok() {
    let s = ObligationSettings::default();
    let none = ladder("b1", &[], 40 * H, &s, Level::Red).unwrap();
    assert_eq!(none.level, Level::Ok);
    assert!(none.notifications.is_empty());
}

#[test]
fn invalid_settings_are_rejected() {
    let s = ObligationSettings {
        delivery_window_millis: 0,
        ..ObligationSettings::default()
    };
    let err = ladder("b1", &[], 0, &s, Level::Ok).unwrap_err();
    assert_eq!(err.code(), "sync_obligations.settings_invalid");
}
