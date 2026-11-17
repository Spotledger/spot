//! The edge_offline module's settings, plan and readiness (WP-RTB-03.T2); no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::edge_offline::{SeriesSetting, Settings, plan};
use spot_edge_offline_host::balance::BalancePolicy;

fn series(name: &str, block: Option<u64>) -> SeriesSetting {
    SeriesSetting {
        name: name.into(),
        block,
    }
}

#[test]
fn defaults_are_a_day_online_only_and_a_thousand() {
    let p = plan(&Settings {
        series: vec![series("INV", None)],
        ..Settings::default()
    })
    .unwrap();
    assert_eq!(p.lease.as_secs(), 24 * 3_600);
    assert_eq!(p.balance, BalancePolicy::OnlineOnly);
    assert_eq!(p.series[0].block, 1_000);
}

#[test]
fn the_lease_is_capped_and_bad_series_are_refused() {
    let p = plan(&Settings {
        identity_lease_hours: Some(9_999),
        ..Settings::default()
    })
    .unwrap();
    assert_eq!(p.lease.as_secs(), 168 * 3_600);
    for bad in [
        vec![series("", None)],
        vec![series("A", Some(0))],
        vec![series("A", None), series("A", None)],
    ] {
        assert!(
            plan(&Settings {
                series: bad,
                ..Settings::default()
            })
            .is_err()
        );
    }
}

#[test]
fn an_unknown_balance_policy_fails_closed() {
    let p = plan(&Settings {
        balances_default: Some("anything".into()),
        ..Settings::default()
    })
    .unwrap();
    assert_eq!(p.balance, BalancePolicy::OnlineOnly);
}
