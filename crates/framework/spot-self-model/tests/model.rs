#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Snapshot construction and incremental events: merge determinism, event chains and refused
//! events.

use spot_agent_tiers::Tier;
use spot_self_model::adapters::{
    HealthAdapter, HealthSignalRecord, SettingRecord, SettingsAdapter,
};
use spot_self_model::model::node_id;
use spot_self_model::{
    Caller, ChangeKind, EvidenceRef, EvidenceSource, MemoryAudit, Node, NodeKind, NodeRef,
    SelfModel, SelfModelError, SelfModelService, SourceAdapter, SourceData, SourceKind,
    SystemEvent,
};

#[test]
fn source_order_never_changes_the_snapshot() {
    let settings = SettingsAdapter::new().with_setting(SettingRecord {
        key: "job.retry_limit".to_owned(),
        scope: "tenant:acme".to_owned(),
        at: 390,
        change: "the retry limit was lowered".to_owned(),
        change_kind: ChangeKind::Updated,
        cause: None,
        evidence: EvidenceRef::at(EvidenceSource::Settings, "setting:1", 390),
    });
    let health = HealthAdapter::new().with_signal(HealthSignalRecord {
        name: "job failing:nightly".to_owned(),
        status: "failing".to_owned(),
        target: NodeRef::new(NodeKind::Job, "nightly"),
        cause: Some(NodeRef::new(NodeKind::Setting, "job.retry_limit")),
        at: 400,
        detail: "the nightly job is failing".to_owned(),
        evidence: EvidenceRef::at(EvidenceSource::Telemetry, "health:1", 400),
    });

    let one = SelfModel::build(9, &[&settings, &health]).expect("the model builds");
    let two = SelfModel::build(9, &[&health, &settings]).expect("the model builds");
    assert_eq!(one, two);
    assert_eq!(one.node_count(), 3);
    assert_eq!(one.source_versions().get(&SourceKind::Settings), Some(&1));
    assert_eq!(one.source_versions().get(&SourceKind::Health), Some(&1));
}

struct Fixed {
    data: SourceData,
}

impl SourceAdapter for Fixed {
    fn source(&self) -> SourceKind {
        SourceKind::Settings
    }

    fn epoch(&self) -> u64 {
        1
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        Ok(self.data.clone())
    }
}

#[test]
fn conflicting_attributes_are_refused() {
    let first = Node::new(
        NodeKind::Setting,
        "job.retry_limit",
        vec![EvidenceRef::at(EvidenceSource::Settings, "setting:1", 1)],
    )
    .expect("the node has evidence")
    .with_attribute("scope", "tenant:acme");
    let second = Node::new(
        NodeKind::Setting,
        "job.retry_limit",
        vec![EvidenceRef::at(EvidenceSource::Settings, "setting:2", 2)],
    )
    .expect("the node has evidence")
    .with_attribute("scope", "tenant:other");
    let one = Fixed {
        data: SourceData {
            nodes: vec![first],
            ..SourceData::new()
        },
    };
    let two = Fixed {
        data: SourceData {
            nodes: vec![second],
            ..SourceData::new()
        },
    };
    let error = SelfModel::build(1, &[&one, &two]).expect_err("conflicting attributes are refused");
    assert_eq!(error.code(), spot_self_model::codes::SOURCE_INVALID);
}

#[test]
fn apply_event_extends_the_causal_chain() {
    let mut model = SelfModel::empty(1);
    let epoch = model
        .apply_event(SystemEvent::SettingChanged {
            key: "job.retry_limit".to_owned(),
            scope: "tenant:acme".to_owned(),
            change: "the retry limit was lowered".to_owned(),
            cause: NodeRef::new(NodeKind::Migration, "upgrade-1"),
            at: 390,
            evidence: EvidenceRef::at(EvidenceSource::Event, "event:1", 390),
        })
        .expect("the setting event applies");
    assert_eq!(epoch, 2);
    assert_eq!(model.epoch(), 2);

    let epoch = model
        .apply_event(SystemEvent::HealthChanged {
            signal: "job failing:nightly".to_owned(),
            status: "failing".to_owned(),
            target: NodeRef::new(NodeKind::Job, "nightly"),
            cause: Some(NodeRef::new(NodeKind::Setting, "job.retry_limit")),
            at: 400,
            evidence: EvidenceRef::at(EvidenceSource::Event, "event:2", 400),
        })
        .expect("the health event applies");
    assert_eq!(epoch, 3);
    assert_eq!(
        model
            .node(&node_id(NodeKind::Setting, "job.retry_limit"))
            .expect("the setting exists")
            .attribute("scope"),
        Some("tenant:acme")
    );
    assert_eq!(
        model
            .history_of(&node_id(NodeKind::Setting, "job.retry_limit"))
            .len(),
        1
    );

    let mut audit = MemoryAudit::new();
    let explanation = SelfModelService::new(&model, &mut audit)
        .why(
            &Caller::new("agent:t1", Tier::T1),
            &NodeRef::new(NodeKind::Job, "nightly"),
            500,
        )
        .expect("the event chain explains the job");
    let kinds: Vec<NodeKind> = explanation.paths[0]
        .steps
        .iter()
        .map(|step| step.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            NodeKind::Job,
            NodeKind::HealthSignal,
            NodeKind::Setting,
            NodeKind::Migration
        ]
    );
    assert!(
        explanation.paths[0]
            .steps
            .iter()
            .skip(1)
            .all(|step| !step.evidence.is_empty() && step.via.is_some())
    );
}

#[test]
fn an_event_without_evidence_is_refused_and_changes_nothing() {
    let mut model = SelfModel::empty(4);
    let error = model
        .apply_event(SystemEvent::NodeChanged {
            node: NodeRef::new(NodeKind::Job, "nightly"),
            change: "nothing".to_owned(),
            cause: NodeRef::new(NodeKind::Setting, "job.retry_limit"),
            at: 1,
            evidence: EvidenceRef::untimed(EvidenceSource::Event, " "),
        })
        .expect_err("an event with no evidence is refused");
    assert_eq!(error.code(), spot_self_model::codes::EVENT_INVALID);
    assert_eq!(model.epoch(), 4);
    assert_eq!(model.node_count(), 0);
}

#[test]
fn a_node_without_evidence_cannot_enter_the_snapshot() {
    let error = Node::new(
        NodeKind::DocType,
        "sales_invoice",
        vec![EvidenceRef::untimed(EvidenceSource::Metadata, "")],
    )
    .expect_err("evidence is required");
    assert_eq!(error.code(), spot_self_model::codes::EVIDENCE_MISSING);
}
