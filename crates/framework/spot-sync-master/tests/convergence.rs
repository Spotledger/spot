//! The acceptance property of WP-EDG-04.T1 (ADR-062 section 6, ADR-065 section 6): a price
//! change reaches 100 simulated branches consistently, branches that were offline when it was
//! published but online before it took effect have it in force, branches offline past its
//! effective instant are reported late, and no branch ever shows the new price before it is
//! effective.
//!
//! Every run is over the real sync state machines and the seeded, fault-injecting network, so a
//! seed replays exactly and a failure names its seed.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

use std::time::Duration;

use spot_sync_master::DEFAULT_BRANCHES;
use spot_sync_master::harness::{self, HarnessConfig, NEW_PRICE, PRIOR_PRICE};

/// The headline acceptance: 100 branches, random outages, and the price change is identical
/// everywhere at the effective instant.
#[test]
fn a_price_change_reaches_one_hundred_branches() {
    let report = harness::run(&HarnessConfig::default()).unwrap();
    assert_eq!(report.branches, DEFAULT_BRANCHES);
    assert_eq!(report.produced, HarnessConfig::default().stream_records);
    assert_eq!(report.on_time + report.late, DEFAULT_BRANCHES);
    assert!(
        report.identical_views,
        "every on-time branch must show the same price at the effective instant"
    );
    assert_eq!(
        report.applied_early, 0,
        "nothing activates before its instant"
    );
    assert!(
        report.dropped > 0,
        "the network should have dropped messages, so the run exercised outages"
    );
    // The case the acceptance names: a branch offline when the change was published but online
    // when it took effect still has it in force at the effective instant.
    assert!(
        report
            .outcomes
            .iter()
            .any(|outcome| !outcome.late && outcome.arrived_at.unix_millis() > 0),
        "at least one branch must have arrived after publication and before the effective instant"
    );
    for outcome in &report.outcomes {
        assert_eq!(
            outcome.view_at_effective.as_deref(),
            Some(if outcome.late { PRIOR_PRICE } else { NEW_PRICE })
        );
    }
}

/// Branches offline past the effective instant are late, keep the prior price at that instant,
/// and their delay is reported.
#[test]
fn a_branch_offline_past_the_effective_instant_is_reported_late() {
    let config = HarnessConfig {
        // A lead so short that no branch can have synced in time.
        lead: Duration::from_millis(1),
        ..HarnessConfig::default()
    };
    let report = harness::run(&config).unwrap();
    assert_eq!(report.on_time, 0);
    assert_eq!(report.late, DEFAULT_BRANCHES);
    assert!(
        report.max_lateness >= Duration::from_millis(1),
        "the lateness must be reported, got {:?}",
        report.max_lateness
    );
    for outcome in report.late_branches() {
        assert_eq!(outcome.view_at_effective.as_deref(), Some(PRIOR_PRICE));
        assert!(outcome.lateness > Duration::ZERO);
    }
}

/// A lead longer than every branch's sync leaves every branch on time and identical.
#[test]
fn a_long_lead_leaves_every_branch_on_time() {
    let config = HarnessConfig {
        lead: Duration::from_secs(60),
        ..HarnessConfig::default()
    };
    let report = harness::run(&config).unwrap();
    assert_eq!(report.on_time, DEFAULT_BRANCHES);
    assert_eq!(report.late, 0);
    assert_eq!(report.max_lateness, Duration::ZERO);
    assert!(report.identical_views);
}

/// The same seed replays exactly, so a reported failure is reproducible.
#[test]
fn the_same_seed_replays_exactly() {
    let config = HarnessConfig {
        branches: 8,
        ..HarnessConfig::default()
    };
    let first = harness::run(&config).unwrap();
    let second = harness::run(&config).unwrap();
    assert_eq!(first, second);
}

/// A branch that received the change early still does not show it before its effective instant.
#[test]
fn an_early_arrival_is_not_applied_early() {
    let config = HarnessConfig {
        lead: Duration::from_secs(30),
        ..HarnessConfig::default()
    };
    let report = harness::run(&config).unwrap();
    assert_eq!(report.applied_early, 0);
    // The long lead means every branch arrived before the effective instant, yet none applied it
    // early: the view at the effective instant is the new price, and at publication it was not.
    assert_eq!(report.on_time, DEFAULT_BRANCHES);
}
