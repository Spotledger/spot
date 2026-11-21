//! CLI renderings round-trip for every operation, and the erase confirmation is literal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mgmt_ops::parse::parse;
use spot_mgmt_ops::{Shell, argv, render};
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
        .validate(&invocation("journal.snapshot.list", &[]))
        .unwrap();
    let back = catalogue
        .validate(&parse(&catalogue, &argv(&validated)).unwrap())
        .unwrap();
    // The default now rides the command line explicitly, so only its origin differs.
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
fn erase_renders_the_confirm_literally_and_no_line_is_a_placeholder() {
    let catalogue = catalogue();
    let validated = catalogue
        .validate(&invocation(
            "journal.subject.erase",
            &[("subject", text("subject-42")), ("confirm", text("erase"))],
        ))
        .unwrap();
    let line = render(&validated, Shell::Posix);
    assert_eq!(
        line,
        "spot ops journal subject erase --subject subject-42 --confirm erase"
    );
    for inv in samples() {
        let line = render(&catalogue.validate(&inv).unwrap(), Shell::Posix);
        assert!(!line.contains("<prompt>"), "{line}");
        assert!(line.starts_with("spot ops journal"), "{line}");
    }
}

#[test]
fn rebuild_run_renders_the_hash_it_executes() {
    let catalogue = catalogue();
    let line = render(
        &catalogue
            .validate(
                samples()
                    .iter()
                    .find(|i| i.op.as_str() == "journal.rebuild.run")
                    .unwrap(),
            )
            .unwrap(),
        Shell::Posix,
    );
    assert!(
        line.contains("--confirm-plan-hash 0123456789abcdef0123456789abcdef"),
        "{line}"
    );
    assert!(line.contains("--target 'at 3:41'"), "{line}");
}
