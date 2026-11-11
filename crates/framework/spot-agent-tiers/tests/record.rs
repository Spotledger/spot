//! The record: every call through `authorise_and_record` leaves exactly one record —
//! allowed or refused, refusals first of all — and no action arguments travel into it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_agent_tiers::{
    Authoriser, Context, Decision, DecisionSink, FinancialRoute, MemorySink, Tier,
};

use common::{approval_by, catalogue, deep_agent, support_agent};

const NOW: i64 = 1_700_000_000;

#[test]
fn out_of_tier_action_refused_and_logged() {
    // The acceptance test: a T3 structural action, unattended, by a plain agent — refused,
    // and the refusal is in the sink.
    let auth = Authoriser::new(catalogue());
    let sink = MemorySink::new();
    let agent = support_agent("agent-normal", Tier::T2);

    let decision = auth.authorise_and_record(&agent, "install_app", &Context::read(), &sink, NOW);
    assert!(
        matches!(decision, Decision::Refuse { .. }),
        "out-of-tier is refused"
    );

    let entries = sink.entries();
    assert_eq!(entries.len(), 1, "exactly one record for one call");
    let record = &entries[0];
    assert_eq!(record.agent, "agent-normal");
    assert_eq!(record.action, "install_app");
    assert_eq!(record.decision_code, "agent.above_agent_max_tier");
    assert_eq!(record.tier, Some(Tier::T3), "the catalogue named the tier");
    assert_eq!(record.at, NOW);
    assert!(!record.decision_reason.is_empty());
}

#[test]
fn refusals_are_recorded_first_of_all() {
    // Even the earliest refusal — an unknown action, before any other check — is recorded.
    let auth = Authoriser::new(catalogue());
    let sink = MemorySink::new();
    let agent = support_agent("agent-normal", Tier::T3);

    let decision =
        auth.authorise_and_record(&agent, "no_such_action", &Context::read(), &sink, NOW);
    assert!(matches!(decision, Decision::Refuse { .. }));
    let entries = sink.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].decision_code, "agent.unknown_action");
    assert_eq!(
        entries[0].tier, None,
        "an unknown action has no tier to name"
    );
}

#[test]
fn allows_are_recorded_too() {
    let auth = Authoriser::new(catalogue());
    let sink = MemorySink::new();
    let agent = support_agent("agent-normal", Tier::T3);

    let decision =
        auth.authorise_and_record(&agent, "explain_failed_job", &Context::read(), &sink, NOW);
    assert!(matches!(decision, Decision::Allow { .. }));
    let entries = sink.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].decision_code, "agent.allow");
    assert_eq!(entries[0].tier, Some(Tier::T0));
}

#[test]
fn every_call_leaves_exactly_one_record_allowed_or_refused() {
    let auth = Authoriser::new(catalogue());
    let sink = MemorySink::new();
    let deep = deep_agent("agent-deep", Tier::T3);

    // A deliberate mix: allows, mid-order refusals, hard-floor refusals.
    let requests = [
        (
            support_agent("agent-a", Tier::T3),
            "explain_failed_job",
            Context::read(),
        ),
        (
            support_agent("agent-b", Tier::T2),
            "install_app",
            Context::read(),
        ),
        (deep.clone(), "install_app", Context::change()),
        (
            support_agent("agent-c", Tier::T3),
            "not_a_thing",
            Context::read(),
        ),
        (
            support_agent("agent-d", Tier::T3),
            "relink_customer_records",
            Context {
                journaled: false,
                ..Context::change()
            },
        ),
        (
            support_agent("agent-e", Tier::T3),
            "relink_customer_records",
            Context {
                targets_submitted_financial: Some(FinancialRoute::Direct),
                ..Context::change()
            },
        ),
    ];
    for (agent, action, ctx) in &requests {
        auth.authorise_and_record(agent, action, ctx, &sink, NOW);
    }
    assert_eq!(
        sink.entries().len(),
        requests.len(),
        "exactly one record per call"
    );
    // Every record's code is either the allow code or one of the refusal codes.
    for entry in sink.entries() {
        assert!(
            entry.decision_code == "agent.allow" || entry.decision_code.starts_with("agent."),
            "the record's code is the stable vocabulary: {}",
            entry.decision_code
        );
    }
}

#[test]
fn the_record_names_the_action_never_its_arguments() {
    // The context (which is where arguments would live) never enters the record; only the
    // action's name does — and the approval's reference, which is evidence, not an
    // argument.
    let auth = Authoriser::new(catalogue());
    let sink = MemorySink::new();
    let agent = support_agent("agent-normal", Tier::T3);
    let ctx = Context {
        human_approval: Some(approval_by("CR-204")),
        ..Context::change()
    };
    let _ = auth.authorise_and_record(&agent, "install_app", &ctx, &sink, NOW);
    let record = &sink.entries()[0];
    assert_eq!(record.approval_reference.as_deref(), Some("CR-204"));

    // A refusal of an invalid approval keeps the reference evidence too.
    let ctx = Context {
        human_approval: Some(approval_by("CR-77")),
        ..Context {
            journaled: false,
            ..Context::read()
        }
    };
    let _ = auth.authorise_and_record(&agent, "install_app", &ctx, &sink, NOW + 1);
    let record = &sink.entries()[1];
    assert_eq!(record.approval_reference.as_deref(), Some("CR-77"));
    assert!(
        !format!("{record:?}").contains("person-ada"),
        "the approver's id is not a record field (only the reference is): {record:?}"
    );
}

#[test]
fn a_custom_sink_receives_the_same_stream() {
    // The trait is the integration point: any sink the host provides gets exactly the
    // records MemorySink would.
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CollectingSink {
        codes: Mutex<Vec<String>>,
    }
    impl DecisionSink for CollectingSink {
        fn record(&self, entry: spot_agent_tiers::DecisionRecord) {
            self.codes.lock().expect("lock").push(entry.decision_code);
        }
    }

    let auth = Authoriser::new(catalogue());
    let sink = Arc::new(CollectingSink::default());
    let agent = support_agent("agent-normal", Tier::T2);
    auth.authorise_and_record(&agent, "install_app", &Context::read(), sink.as_ref(), NOW);
    auth.authorise_and_record(
        &agent,
        "explain_failed_job",
        &Context::read(),
        sink.as_ref(),
        NOW,
    );
    assert_eq!(
        *sink.codes.lock().expect("lock"),
        vec!["agent.above_agent_max_tier", "agent.allow"]
    );
}
