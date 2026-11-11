//! The CI gate: a stored-shape change with no upcaster fails.
//!
//! A patch that renames, drops, retypes, moves or requires a stored field must ship the upcaster
//! that rewrites the images written before the change (ADR-061 s6). `check_patch` is the rule;
//! `check_shapes` is the step CI runs, and this file proves both halves: today's table covers
//! every change the releases declared, an additive change needs nothing, a change with no step is
//! reported with the entity and the versions, and the binary exits non-zero when it is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use spot_image_upcast::{ShapeChange, ShapeChangeKind, check_patch};
use spot_journal_compat::{declared_shape_changes, gate_findings, gate_findings_omitting, today};

#[test]
fn every_declared_shape_change_of_the_releases_has_an_upcaster() {
    let findings = gate_findings().expect("today's table registers");
    assert!(
        findings.is_empty(),
        "a patch shipped a shape change with no upcaster: {findings:?}"
    );
    assert!(
        !declared_shape_changes().is_empty(),
        "the gate is only meaningful while the releases declare changes"
    );
}

#[test]
fn an_additive_shape_change_needs_no_upcaster() {
    let registry = today().expect("today's table registers");
    let additive = [
        ShapeChange::new("delivery_note", 1, 2, ShapeChangeKind::DocTypeAdded),
        ShapeChange::new("invoice", 3, 4, ShapeChangeKind::FieldAdded),
    ];
    assert!(
        check_patch(&registry, &additive).is_empty(),
        "a new entity and a new optional field read under the new schema unchanged"
    );
}

#[test]
fn a_shape_change_without_an_upcaster_names_entity_and_versions() {
    let registry = today().expect("today's table registers");
    // A patch that renames a field on `invoice` from schema 1 to 2 while no step covers that hop:
    // the invoice's chain starts at 2, so an image written under schema 1 can never reach it.
    let simulated = [ShapeChange::new(
        "invoice",
        1,
        2,
        ShapeChangeKind::FieldRenamed,
    )];
    let findings = check_patch(&registry, &simulated);
    assert_eq!(findings.len(), 1, "one row, one finding");
    let finding = &findings[0];
    assert_eq!(finding.entity(), "invoice");
    assert_eq!(finding.kind(), ShapeChangeKind::FieldRenamed);
    assert_eq!(finding.from(), 1, "the schema before the change");
    assert_eq!(finding.to(), 2, "the schema after it");
    assert_eq!(
        finding.missing_from(),
        1,
        "the chain stops at the first missing step"
    );
    assert_eq!(
        finding.missing_to(),
        2,
        "which has to reach the change's own target"
    );
    let text = finding.to_string();
    for expected in ["invoice", "field_renamed", "schema 1 to schema 2"] {
        assert!(text.contains(expected), "{text} names {expected}");
    }
}

#[test]
fn the_gate_reports_the_step_the_table_is_missing() {
    // The same question the binary answers, asked in process: omit the invoice's step and the
    // two changes that rely on it are uncovered.
    let findings = gate_findings_omitting(&[("invoice", 2)]).expect("the trimmed table registers");
    assert_eq!(
        findings.len(),
        1,
        "one declared change needs the omitted step"
    );
    assert_eq!(findings[0].entity(), "invoice");
    assert_eq!(findings[0].missing_from(), 2);
    assert_eq!(findings[0].missing_to(), 3);
}

#[test]
fn the_gate_binary_passes_today_and_fails_when_a_step_is_missing() {
    let passing = Command::new(env!("CARGO_BIN_EXE_check_shapes"))
        .output()
        .expect("the gate binary runs");
    assert!(
        passing.status.success(),
        "the gate must pass on today's table: {}",
        String::from_utf8_lossy(&passing.stderr)
    );
    assert!(
        String::from_utf8_lossy(&passing.stdout)
            .contains("every declared shape change has an upcaster"),
        "the gate says what it checked"
    );

    let failing = Command::new(env!("CARGO_BIN_EXE_check_shapes"))
        .args(["--omit", "invoice:2"])
        .output()
        .expect("the gate binary runs");
    assert_eq!(
        failing.status.code(),
        Some(1),
        "a shape change with no upcaster fails the step"
    );
    let stdout = String::from_utf8_lossy(&failing.stdout);
    for expected in ["invoice", "schema 2 to schema 3"] {
        assert!(stdout.contains(expected), "{stdout} names {expected}");
    }
}
