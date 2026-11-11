//! Acknowledged drift: a reason, an owner, an audit trail and an expiry that holds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use spot_desired_state::{
    DesiredFleet, DriftSubject, ExemptionAction, ExemptionBook, ExemptionRequest, MemoryAudit,
    RemediationPolicy, detect_drift, detect_unexempted, remediate,
};
use support::{group_state, key, observed, registry, server, servers};

fn subject() -> DriftSubject {
    DriftSubject::Setting(key("runtime.logging.level"))
}

#[test]
fn an_exemption_hides_a_drift_until_the_exact_expiry_instant() {
    let members = servers(100);
    let desired = DesiredFleet::new(registry(), vec![group_state(&members)]).unwrap();
    let drifted: BTreeSet<usize> = [7, 42, 91].into_iter().collect();
    let reports = observed(&members, &drifted);
    assert_eq!(
        detect_drift(&desired, &reports, support::NOW, support::DEADLINE).len(),
        3
    );

    let mut audit = MemoryAudit::new();
    let mut book = ExemptionBook::new();
    book.acknowledge(
        ExemptionRequest::new(
            server(7),
            subject(),
            "the log level is pinned for the certification run",
            "alice",
            2_000,
        ),
        1_000,
        &mut audit,
    )
    .unwrap();
    assert_eq!(audit.events()[0].action, ExemptionAction::Acknowledged);

    assert_eq!(
        detect_unexempted(&desired, &reports, &book, 1_999, support::DEADLINE).len(),
        2,
        "one second before expiry the acknowledgement still holds"
    );
    assert_eq!(
        detect_unexempted(&desired, &reports, &book, 2_000, support::DEADLINE).len(),
        3,
        "at the expiry instant the drift alarms again"
    );

    let plan = remediate(
        &detect_unexempted(&desired, &reports, &book, 1_999, support::DEADLINE),
        &RemediationPolicy::standard(),
    );
    assert_eq!(
        plan.size(),
        2,
        "an exempted drift is not remediated by accident"
    );

    let removed = book.purge_expired(2_000, &mut audit);
    assert_eq!(removed.len(), 1);
    assert_eq!(audit.len(), 2);
    assert_eq!(audit.events()[1].action, ExemptionAction::Expired);
}

#[test]
fn a_revoked_exemption_stops_covering_the_drift() {
    let mut audit = MemoryAudit::new();
    let mut book = ExemptionBook::new();
    book.acknowledge(
        ExemptionRequest::new(server(42), subject(), "temporary debugging", "alice", 9_000),
        1_000,
        &mut audit,
    )
    .unwrap();
    assert!(book.is_exempt(&server(42), &subject(), 1_500));

    assert!(book.revoke(&server(42), &subject(), "bob", 1_600, &mut audit));
    assert!(!book.is_exempt(&server(42), &subject(), 1_700));
    assert!(!book.revoke(&server(42), &subject(), "bob", 1_800, &mut audit));
    assert_eq!(audit.events().len(), 2);
    assert_eq!(audit.events()[1].action, ExemptionAction::Revoked);
    assert_eq!(audit.events()[1].actor, "bob");
}

#[test]
fn an_exemption_needs_a_reason_an_owner_and_a_future_expiry() {
    let mut audit = MemoryAudit::new();
    let mut book = ExemptionBook::new();

    let no_reason = book.acknowledge(
        ExemptionRequest::new(server(1), subject(), "  ", "alice", 2_000),
        1_000,
        &mut audit,
    );
    assert_eq!(
        no_reason.unwrap_err().code(),
        "desired_state.invalid_exemption"
    );

    let no_owner = book.acknowledge(
        ExemptionRequest::new(server(1), subject(), "why", "", 2_000),
        1_000,
        &mut audit,
    );
    assert_eq!(
        no_owner.unwrap_err().code(),
        "desired_state.invalid_exemption"
    );

    let already_expired = book.acknowledge(
        ExemptionRequest::new(server(1), subject(), "why", "alice", 1_000),
        1_000,
        &mut audit,
    );
    assert_eq!(
        already_expired.unwrap_err().code(),
        "desired_state.invalid_exemption"
    );
    assert!(book.is_empty());
    assert!(audit.is_empty());
}

#[test]
fn an_exemption_is_per_server_and_subject() {
    let mut audit = MemoryAudit::new();
    let mut book = ExemptionBook::new();
    book.acknowledge(
        ExemptionRequest::new(server(7), subject(), "pinned", "alice", 2_000),
        1_000,
        &mut audit,
    )
    .unwrap();

    assert!(book.is_exempt(&server(7), &subject(), 1_500));
    assert!(!book.is_exempt(&server(8), &subject(), 1_500));
    assert!(!book.is_exempt(
        &server(7),
        &DriftSubject::Package("nginx".to_owned()),
        1_500
    ));
    assert_eq!(book.active(1_500).len(), 1);
    assert!(book.active(2_000).is_empty());
}
