//! The decision sink: an out-of-tier action is refused and recorded, and a full channel refuses
//! the action under the fail-closed policy (and only counts the drop under fail-open).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::Recording;
use spot_agent_tiers::{ActionCatalogue, AgentIdentity, Authoriser, Context, Domain, Tier};
use spot_audit_chain::{AuditChain, CheckpointPolicy, LocalSigner, PayloadValue};
use spot_audit_store::{
    AgentDecisionAudit, AuditedAuthoriser, OverflowPolicy, audit_record_of, drain,
};

const CATALOGUE: &str = r#"
[[actions]]
name = "explain_failed_job"
tier = "T0"
domain = "support"
effect = "read"

[[actions]]
name = "change_print_format"
tier = "T1"
domain = "support"
effect = "reversible"
"#;

fn authoriser() -> Authoriser {
    let catalogue = ActionCatalogue::from_toml(CATALOGUE).expect("the catalogue loads");
    Authoriser::new(catalogue)
}

fn agent() -> AgentIdentity {
    // A T0 support agent: `change_print_format` is T1, out of its tier.
    AgentIdentity::new("agent-1", "owner-1", Domain::Support, Tier::T0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_tier_action_is_refused_and_its_record_in_the_chain() {
    let Some(db) = common::db().await else { return };
    let telemetry = Arc::new(Recording::new());
    let (sink, mut receiver) = AgentDecisionAudit::new(16, telemetry.clone());
    let sink = Arc::new(sink);
    let wrapper = AuditedAuthoriser::new(
        authoriser(),
        Arc::clone(&sink),
        OverflowPolicy::FailClosed,
        telemetry,
    );

    let decision = wrapper
        .authorise(&agent(), "change_print_format", &Context::read(), 100)
        .expect("the channel has room");
    assert!(
        matches!(decision, spot_agent_tiers::Decision::Refuse { .. }),
        "an out-of-tier action must be refused"
    );

    // Drain the record into the chain and read it back.
    let chain = AuditChain::new(
        common::store(&db).await,
        "sink",
        Arc::new(LocalSigner::from_seed([1; 32])),
        CheckpointPolicy { every: 0 },
    );
    let appended = drain(&mut receiver, &chain, 1).await.expect("drain");
    assert_eq!(appended, 1);
    let entries = chain.read_range(1, 1).await.expect("read");
    assert_eq!(entries.len(), 1);
    let record = &entries[0].record;
    assert_eq!(record.kind, "agent.decision");
    assert_eq!(record.actor, "agent:agent-1");
    assert!(record.masked, "a decision record is always masked");
    match record.payload.get("decision") {
        Some(PayloadValue::Text(code)) => {
            assert!(code.starts_with("agent."), "{code}");
            assert_ne!(code, "agent.allow", "the refusal is recorded as a refusal");
        }
        other => panic!("no decision code: {other:?}"),
    }
    // The action's arguments never appear.
    assert!(!record.payload.contains_key("args"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_channel_refuses_under_fail_closed_and_counts_under_fail_open() {
    let telemetry = Arc::new(Recording::new());
    // Capacity one: one record fills it.
    let (sink, _receiver) = AgentDecisionAudit::new(1, telemetry.clone());
    let sink = Arc::new(sink);

    let closed = AuditedAuthoriser::new(
        authoriser(),
        Arc::clone(&sink),
        OverflowPolicy::FailClosed,
        telemetry.clone(),
    );
    closed
        .authorise(&agent(), "explain_failed_job", &Context::read(), 1)
        .expect("the first record fits");
    let refused = closed
        .authorise(&agent(), "explain_failed_job", &Context::read(), 2)
        .expect_err("the full channel refuses the action");
    assert_eq!(refused.code(), "auditstore.unavailable");
    assert_eq!(telemetry.dropped(), 1, "the drop is counted");

    let open = AuditedAuthoriser::new(
        authoriser(),
        Arc::clone(&sink),
        OverflowPolicy::FailOpen,
        telemetry.clone(),
    );
    open.authorise(&agent(), "explain_failed_job", &Context::read(), 3)
        .expect("fail-open runs the action");
    assert_eq!(
        telemetry.dropped(),
        2,
        "the drop is counted but the action ran"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sink_counts_a_drop_when_used_directly() {
    let telemetry = Arc::new(Recording::new());
    let (sink, _receiver) = AgentDecisionAudit::new(1, telemetry.clone());
    use spot_agent_tiers::DecisionSink;
    let record = |at: i64| spot_agent_tiers::DecisionRecord {
        agent: "agent-1".to_owned(),
        action: "explain_failed_job".to_owned(),
        tier: None,
        decision_code: "agent.allow".to_owned(),
        decision_reason: "ok".to_owned(),
        approval_reference: None,
        at,
    };
    sink.record(record(1));
    sink.record(record(2));
    assert_eq!(telemetry.dropped(), 1);
    let _ = audit_record_of(&record(3));
}
