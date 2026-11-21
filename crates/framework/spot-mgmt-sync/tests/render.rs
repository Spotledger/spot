//! CLI renderings round-trip for every operation, and the confirmations are literal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mgmt_ops::parse::parse;
use spot_mgmt_ops::{Shell, argv, render};
use spot_mgmt_sync::ids;
use support::*;

#[test]
fn every_operation_round_trips_through_the_command_line() {
    let catalogue = catalogue();
    let samples = samples();
    assert_eq!(samples.len(), catalogue.len());
    for inv in samples {
        let validated = catalogue.validate(&inv).unwrap();
        let args = argv(&validated);
        let parsed = parse(&catalogue, &args).unwrap();
        let back = catalogue.validate(&parsed).unwrap();
        assert_eq!(back, validated, "{}", inv.op.as_str());
    }
}

#[test]
fn defaults_are_filled_and_round_trip_too() {
    let catalogue = catalogue();
    let validated = catalogue
        .validate(&invocation(
            ids::NODE_ENROL,
            &[
                ("node", text("branch07")),
                ("partitions", list(&["sales"])),
                ("subscription", text("branch-standard")),
            ],
        ))
        .unwrap();
    let back = catalogue
        .validate(&parse(&catalogue, &argv(&validated)).unwrap())
        .unwrap();
    let values = |v: &spot_mgmt_ops::Validated| {
        v.params
            .iter()
            .map(|p| (p.name.clone(), p.value.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(back.op, validated.op);
    assert_eq!(values(&back), values(&validated));
}

#[test]
fn enrol_renders_one_flag_per_partition_and_no_secret() {
    let catalogue = catalogue();
    let enrol = samples()
        .into_iter()
        .find(|i| i.op.as_str() == ids::NODE_ENROL)
        .unwrap();
    let line = render(&catalogue.validate(&enrol).unwrap(), Shell::Posix);
    assert!(line.starts_with("spot ops sync node enrol"), "{line}");
    assert!(line.contains("--partitions sales"), "{line}");
    assert!(line.contains("--partitions stock"), "{line}");
    assert!(line.contains("--valid-hours 48"), "{line}");
    assert!(!line.contains("<prompt>"), "{line}");
    assert!(!line.contains("code"), "{line}");
}

#[test]
fn destructive_operations_render_their_confirm_literally() {
    let catalogue = catalogue();
    let line_of = |op: &str| {
        let inv = samples().into_iter().find(|i| i.op.as_str() == op).unwrap();
        render(&catalogue.validate(&inv).unwrap(), Shell::Posix)
    };
    assert_eq!(
        line_of(ids::NODE_REVOKE),
        "spot ops sync node revoke --node branch07.spotledger.app --confirm branch07.spotledger.app"
    );
    assert_eq!(
        line_of(ids::ROLE_CHANGE),
        "spot ops sync role change --to hub --confirm hub"
    );
    assert!(line_of(ids::QUARANTINE_DISCARD).ends_with("--confirm q-0192f1c0"));
}

#[test]
fn every_line_starts_with_the_sync_command() {
    let catalogue = catalogue();
    for inv in samples() {
        let line = render(&catalogue.validate(&inv).unwrap(), Shell::Posix);
        assert!(line.starts_with("spot ops sync"), "{line}");
    }
}
