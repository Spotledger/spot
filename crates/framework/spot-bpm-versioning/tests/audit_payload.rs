//! `MigrationAudit::to_payload` against the `sys_bpm_case_migrated` contract (WP-CON-04.T8).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use serde_json::{Value as Json, json};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{
    CaseId, CaseRecord, EffectLedger, HandlerRegistry, Harness, NodeKind, ProcessDefinition,
    RecordingHandler,
};
use spot_bpm_model::{ProcessVersion, from_text};
use spot_bpm_versioning::{
    MIGRATION_AUDIT_TOPIC, MemoryMigrationTarget, MigrationAudit, MigrationMapping, VersionRef,
    plan_migration,
};
use spot_dag::NodeId;
use spot_events_contract::system;
use spot_events_contract::system_platform::BPM_CASE_MIGRATED_TOPIC;

const EXAMPLE: &str =
    include_str!("../../../../contracts/events/examples/sys/sys_bpm_case_migrated.json");

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

const V1: &str = r#"
name = "audit_order"
app = "audit_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "review"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "review"
"#;

const V2: &str = r#"
name = "audit_order"
app = "audit_test"
version = 2
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "approve"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "approve"
"#;

fn parse(text: &str) -> ProcessVersion {
    from_text(text).expect("the process text must parse")
}

fn definition(version: &ProcessVersion) -> ProcessDefinition {
    ProcessDefinition::compile(version).expect("the process must compile")
}

fn case_at_review(name: &str) -> CaseRecord {
    let ledger = Arc::new(EffectLedger::new());
    let mut registry = HandlerRegistry::logic_default();
    registry.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
    let harness = Harness::with_ledger(registry, ledger);
    let case = CaseId::new(name).unwrap();
    block_on(harness.start(&parse(V1), case.clone(), name)).unwrap();
    block_on(harness.step(&case)).unwrap();
    block_on(harness.engine().load(&case)).unwrap()
}

/// The audit a real migration writes through the memory target.
fn migrated_audit(actor: Option<&str>) -> MigrationAudit {
    let record = case_at_review("audit_case");
    let mapping = MigrationMapping::new().with_node(
        NodeId::new("review").unwrap(),
        NodeId::new("approve").unwrap(),
    );
    let plan = plan_migration(
        &record,
        &definition(&parse(V1)),
        &definition(&parse(V2)),
        &mapping,
    )
    .unwrap();
    let target = MemoryMigrationTarget::new().with_case(record);
    let applied = plan.apply(&target, now(), actor).unwrap();
    let audits = target.audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0], applied.audit);
    applied.audit
}

fn hex(c: char) -> String {
    c.to_string().repeat(64)
}

fn handmade(actor: Option<&str>, nodes: usize, defaulted: usize) -> MigrationAudit {
    MigrationAudit {
        topic: MIGRATION_AUDIT_TOPIC.to_owned(),
        case: CaseId::new("case_0042").unwrap(),
        from: VersionRef {
            name: "purchase_approval".to_owned(),
            version: 3,
            hash: hex('b'),
        },
        to: VersionRef {
            name: "purchase_approval".to_owned(),
            version: 4,
            hash: hex('c'),
        },
        plan_digest: hex('a'),
        actor: actor.map(str::to_owned),
        store_version: 7,
        nodes_changed: nodes,
        variables_defaulted: defaulted,
        at: OffsetDateTime::from_unix_timestamp(1_793_608_157).unwrap(),
    }
}

/// The type of every leaf, keys kept: the shape of a payload.
fn shape(value: &Json) -> Json {
    match value {
        Json::Object(map) => Json::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        Json::String(_) => json!("string"),
        Json::Number(n) if n.is_f64() => json!("float"),
        Json::Number(_) => json!("integer"),
        Json::Bool(_) => json!("bool"),
        Json::Null => json!("null"),
        Json::Array(_) => json!("array"),
    }
}

fn assert_contract_accepts(audit: &MigrationAudit) {
    let payload = audit.to_payload();
    if let Err(errors) = system::validate(BPM_CASE_MIGRATED_TOPIC, &payload) {
        panic!("the contract refused {payload}: {errors:?}");
    }
}

#[test]
fn the_audit_topic_is_the_contracts_topic() {
    assert_eq!(MIGRATION_AUDIT_TOPIC, BPM_CASE_MIGRATED_TOPIC);
    assert_eq!(migrated_audit(None).topic, BPM_CASE_MIGRATED_TOPIC);
}

#[test]
fn a_migrated_cases_audit_is_accepted_by_the_contract() {
    assert_contract_accepts(&migrated_audit(Some("user_1")));
    assert_contract_accepts(&migrated_audit(None));
}

#[test]
fn every_field_round_trips() {
    let audit = migrated_audit(Some("user_1"));
    let p = audit.to_payload();
    assert_eq!(p["case"], json!(audit.case.to_string()));
    assert_eq!(p["case"], json!("audit_case"));
    for (side, pin) in [("from", &audit.from), ("to", &audit.to)] {
        assert_eq!(p[side]["name"], json!(pin.name));
        assert_eq!(p[side]["version"], json!(pin.version));
        assert_eq!(p[side]["hash"], json!(pin.hash));
    }
    assert_eq!(p["from"]["version"], json!(1));
    assert_eq!(p["to"]["version"], json!(2));
    assert_eq!(p["plan_digest"], json!(audit.plan_digest));
    assert_eq!(p["actor"], json!("user_1"));
    assert_eq!(p["store_version"], json!(audit.store_version));
    assert_eq!(p["nodes_changed"], json!(1));
    assert_eq!(p["variables_defaulted"], json!(0));
    assert_eq!(p["at"], json!(1_700_000_000_000_000_i64));
    assert_eq!(p.as_object().unwrap().len(), 9);
}

#[test]
fn time_is_unix_microseconds_and_sub_microseconds_are_dropped() {
    let mut audit = handmade(None, 1, 0);
    audit.at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_789).unwrap();
    assert_eq!(audit.to_payload()["at"], json!(1_700_000_000_123_456_i64));
    assert_contract_accepts(&audit);
}

#[test]
fn the_outbox_text_is_deterministic_sorted_and_parses_back() {
    let audit = handmade(Some("user_1"), 2, 1);
    let first = audit.to_outbox_json();
    assert_eq!(first, audit.clone().to_outbox_json());
    assert!(first.starts_with(
        r#"{"actor":"user_1","at":1793608157000000,"case":"case_0042","from":{"hash":""#
    ));
    assert!(first.contains(r#""nodes_changed":2,"plan_digest":""#));
    assert!(first.ends_with(r#""variables_defaulted":1}"#));
    assert!(!first.contains(' '));
    let parsed: Json = serde_json::from_str(&first).unwrap();
    assert_eq!(parsed, audit.to_payload());
}

#[test]
fn an_absent_actor_is_omitted() {
    let audit = handmade(None, 2, 1);
    let payload = audit.to_payload();
    assert!(payload.get("actor").is_none());
    assert!(!audit.to_outbox_json().contains("actor"));
    assert_contract_accepts(&audit);
}

#[test]
fn a_zero_count_migration_is_valid() {
    let audit = handmade(Some("user_1"), 0, 0);
    let payload = audit.to_payload();
    assert_eq!(payload["nodes_changed"], json!(0));
    assert_eq!(payload["variables_defaulted"], json!(0));
    assert_contract_accepts(&audit);
}

#[test]
fn no_float_appears_anywhere() {
    let payload = migrated_audit(Some("user_1")).to_payload();
    assert!(!shape(&payload).to_string().contains("float"));
}

#[test]
fn the_contracts_example_has_the_same_fields_and_types() {
    let example: Json = serde_json::from_str(EXAMPLE).unwrap();
    system::validate(BPM_CASE_MIGRATED_TOPIC, &example).unwrap();
    let mut ours = handmade(Some("user_1"), 2, 1);
    ours.at = OffsetDateTime::from_unix_timestamp_nanos(1_793_608_157_957_000_000).unwrap();
    assert_eq!(shape(&ours.to_payload()), shape(&example));
    // The example's values are exactly what this audit writes.
    assert_eq!(ours.to_payload(), example);
}
