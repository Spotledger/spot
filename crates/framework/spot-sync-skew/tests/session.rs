//! Tests for session.
mod common;

use common::report;
use spot_sync_skew::{
    Decision, NODE_TOO_OLD, Retry, SESSION_EPOCH_UNVERIFIED_HUB, SESSION_EPOCH_UNVERIFIED_NODE,
    SESSION_MIGRATING_HUB, SESSION_MIGRATING_NODE, Settings, SoftwareVersion, releases_behind,
    session_decision,
};

fn refuse(key: &'static str, retry: Retry) -> Decision {
    Decision::Refuse {
        reason_key: key,
        retry,
    }
}

#[test]
fn truth_table() {
    let s = Settings::default();
    let hub = report(5);
    // healthy, level
    assert_eq!(session_decision(&hub, &report(5), s), Decision::Run);
    // hub mid-migration / unverified
    let mut h = report(5);
    h.mid_migration = true;
    assert_eq!(
        session_decision(&h, &report(5), s),
        refuse(SESSION_MIGRATING_HUB, Retry::Wait)
    );
    let mut h = report(5);
    h.epoch_verified = false;
    assert_eq!(
        session_decision(&h, &report(5), s),
        refuse(SESSION_EPOCH_UNVERIFIED_HUB, Retry::Wait)
    );
    // node mid-migration / unverified
    let mut n = report(5);
    n.mid_migration = true;
    assert_eq!(
        session_decision(&hub, &n, s),
        refuse(SESSION_MIGRATING_NODE, Retry::Wait)
    );
    let mut n = report(5);
    n.epoch_verified = false;
    assert_eq!(
        session_decision(&hub, &n, s),
        refuse(SESSION_EPOCH_UNVERIFIED_NODE, Retry::Wait)
    );
    // a too-old node on an unverified epoch still gets the upgrade channel, never a bare wait
    let mut n = report(1);
    n.epoch_verified = false;
    assert_eq!(
        session_decision(&hub, &n, s),
        refuse(NODE_TOO_OLD, Retry::UpgradeChannelOnly)
    );
    // a too-old node that is also migrating waits first
    let mut n = report(1);
    n.mid_migration = true;
    assert_eq!(
        session_decision(&hub, &n, s),
        refuse(SESSION_MIGRATING_NODE, Retry::Wait)
    );
}

#[test]
fn skew_at_the_limit_runs_and_one_beyond_is_too_old() {
    let s = Settings::default();
    let hub = report(5);
    assert_eq!(session_decision(&hub, &report(4), s), Decision::Run);
    assert_eq!(
        session_decision(&hub, &report(3), s),
        refuse(NODE_TOO_OLD, Retry::UpgradeChannelOnly)
    );
    let wide = Settings {
        allowed_release_skew: 2,
    };
    assert_eq!(session_decision(&hub, &report(3), wide), Decision::Run);
    let none = Settings {
        allowed_release_skew: 0,
    };
    assert_eq!(
        session_decision(&hub, &report(4), none),
        refuse(NODE_TOO_OLD, Retry::UpgradeChannelOnly)
    );
}

#[test]
fn releases_behind_counts_minor_steps_and_whole_majors() {
    let v = SoftwareVersion::new;
    assert_eq!(releases_behind(v(2, 5, 9), v(2, 5, 0)), 0);
    assert_eq!(releases_behind(v(2, 5, 0), v(2, 3, 7)), 2);
    assert_eq!(releases_behind(v(3, 0, 0), v(2, 9, 0)), 1);
    assert_eq!(releases_behind(v(4, 0, 0), v(2, 9, 0)), 2);
    // across a major the hub's own shipped minors count, so a whole major plus five minors is 6
    assert_eq!(releases_behind(v(2, 5, 0), v(1, 0, 0)), 6);
    // a node ahead of the hub is not behind
    assert_eq!(releases_behind(v(2, 1, 0), v(2, 4, 0)), 0);
    assert_eq!(releases_behind(v(2, 1, 0), v(3, 0, 0)), 0);
}
