#![allow(clippy::expect_used, clippy::unwrap_used)]

//! The read-only introspection API: tier filtering, the audit record and the time window.

use std::sync::Arc;

use spot_agent_tiers::Tier;
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Property, Registry};
use spot_self_model::adapters::{MetadataAdapter, SettingRecord, SettingsAdapter};
use spot_self_model::{
    Caller, ChangeKind, EdgeKind, EvidenceRef, EvidenceSource, MemoryAudit, NodeKind, NodeRef,
    Provenance, QueryKind, QueryOutcome, QueryRecord, SelfModel, SelfModelError, SelfModelService,
    TimeRange,
};

fn metadata() -> MetadataAdapter {
    let mut definition = Definition::new("sales_invoice");
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    definition.fields = vec![total];
    let mut customisations = Customisations::new();
    customisations.set("sales_invoice", Some("total"), Property::Required(true));
    let meta = Registry::build(vec![definition], &customisations).expect("the metadata builds");
    MetadataAdapter::new(Arc::new(meta), customisations).with_provenance(
        "override:sales_invoice.total.required",
        Provenance::new("user:alice", 100, "promotion:7"),
    )
}

fn settings() -> SettingsAdapter {
    SettingsAdapter::new().with_setting(SettingRecord {
        key: "job.retry_limit".to_owned(),
        scope: "tenant:acme".to_owned(),
        at: 390,
        change: "the retry limit was lowered".to_owned(),
        change_kind: ChangeKind::Updated,
        cause: None,
        evidence: EvidenceRef::at(EvidenceSource::Settings, "setting:1", 390),
    })
}

fn model() -> (SelfModel, MemoryAudit) {
    let metadata = metadata();
    let settings = settings();
    let built = SelfModel::build(5, &[&metadata, &settings]).expect("the model builds");
    (built, MemoryAudit::new())
}

/// The same snapshot with the setting left out: `Setting:job.retry_limit` is absent from it and
/// every other node is the one [`model`] holds.
fn model_without_the_setting() -> SelfModel {
    let metadata = metadata();
    SelfModel::build(5, &[&metadata]).expect("the model builds")
}

#[test]
fn describe_lists_neighbours_history_and_audits_the_query() {
    let (model, mut audit) = model();
    let caller = Caller::new("agent:t0", Tier::T0);
    let field = NodeRef::new(NodeKind::Field, "sales_invoice.total");
    let description = SelfModelService::new(&model, &mut audit)
        .describe(&caller, &field, 500)
        .expect("T0 may describe a field");

    assert_eq!(description.node.kind, NodeKind::Field);
    assert!(description.neighbours.iter().any(|neighbour| {
        neighbour.kind == EdgeKind::Defines
            && neighbour.direction == spot_self_model::Direction::Incoming
            && neighbour.node.kind == NodeKind::DocType
    }));
    assert!(description.neighbours.iter().any(|neighbour| {
        neighbour.kind == EdgeKind::Overrides && neighbour.node.kind == NodeKind::Customisation
    }));
    assert_eq!(description.history.len(), 1);
    assert_eq!(description.history[0].at, 100);

    assert_eq!(audit.records().len(), 1);
    let record = &audit.records()[0];
    assert_eq!(record.query, QueryKind::Describe);
    assert_eq!(record.caller, "agent:t0");
    assert_eq!(record.tier, Tier::T0);
    assert_eq!(record.subject.as_deref(), Some("Field:sales_invoice.total"));
    assert_eq!(record.outcome, QueryOutcome::Allowed);
    assert_eq!(record.at, 500);
}

#[test]
fn neighbors_filter_by_edge_kind() {
    let (model, mut audit) = model();
    let caller = Caller::new("agent:t0", Tier::T0);
    let field = NodeRef::new(NodeKind::Field, "sales_invoice.total");
    let neighbours = SelfModelService::new(&model, &mut audit)
        .neighbors(&caller, &field, &[EdgeKind::Overrides], 500)
        .expect("T0 may list overrides");

    assert_eq!(neighbours.len(), 1);
    assert_eq!(neighbours[0].kind, EdgeKind::Overrides);
    assert_eq!(neighbours[0].node.kind, NodeKind::Customisation);
    assert_eq!(neighbours[0].at, Some(100));
    assert!(!neighbours[0].evidence.is_empty());
}

#[test]
fn history_honours_its_window_and_refuses_an_inverted_one() {
    let (model, mut audit) = model();
    let caller = Caller::new("agent:t0", Tier::T0);
    let field = NodeRef::new(NodeKind::Field, "sales_invoice.total");
    let mut service = SelfModelService::new(&model, &mut audit);

    let inside = service
        .history(&caller, &field, &TimeRange::new(90, 110), 500)
        .expect("the entry is inside the window");
    assert_eq!(inside.len(), 1);
    assert_eq!(inside[0].change, ChangeKind::Updated);

    let outside = service
        .history(&caller, &field, &TimeRange::new(200, 300), 500)
        .expect("the window is valid");
    assert!(outside.is_empty());

    let error = service
        .history(&caller, &field, &TimeRange::new(110, 90), 500)
        .expect_err("an inverted window is refused");
    assert_eq!(error.code(), spot_self_model::codes::SOURCE_INVALID);
    assert_eq!(audit.records().len(), 3);
    assert_eq!(audit.records()[2].outcome, QueryOutcome::Failed);
}

#[test]
fn a_t0_caller_cannot_see_settings_but_a_t1_caller_can() {
    let (model, mut audit) = model();
    let setting = NodeRef::new(NodeKind::Setting, "job.retry_limit");
    let t0 = Caller::new("agent:t0", Tier::T0);
    let denied = {
        let mut service = SelfModelService::new(&model, &mut audit);
        service
            .describe(&t0, &setting, 500)
            .expect_err("a setting is T1 configuration")
    };
    // The caller is told the node is absent; the tier refusal stays in the audit record.
    assert_eq!(denied.code(), spot_self_model::codes::NODE_NOT_FOUND);
    assert_eq!(audit.records()[0].outcome, QueryOutcome::Denied);

    let t1 = Caller::new("agent:t1", Tier::T1);
    let described = {
        let mut service = SelfModelService::new(&model, &mut audit);
        service
            .describe(&t1, &setting, 500)
            .expect("T1 may describe a setting")
    };
    assert_eq!(described.node.kind, NodeKind::Setting);
    assert_eq!(audit.records()[1].outcome, QueryOutcome::Allowed);
}

#[test]
fn why_filters_hidden_causes_and_unknown_subjects() {
    let (model, mut audit) = model();
    let setting = NodeRef::new(NodeKind::Setting, "job.retry_limit");
    let t0 = Caller::new("agent:t0", Tier::T0);
    let mut service = SelfModelService::new(&model, &mut audit);

    let denied = service
        .why(&t0, &setting, 500)
        .expect_err("the subject itself is hidden");
    // A hidden subject and an absent one answer the same, so the subject's existence stays hidden.
    assert_eq!(denied.code(), spot_self_model::codes::NODE_NOT_FOUND);

    let missing = NodeRef::new(NodeKind::Job, "nightly");
    let absent = service
        .why(&t0, &missing, 500)
        .expect_err("the subject does not exist");
    assert_eq!(absent.code(), spot_self_model::codes::NODE_NOT_FOUND);

    assert_eq!(audit.records()[0].outcome, QueryOutcome::Denied);
    assert_eq!(audit.records()[1].outcome, QueryOutcome::NotFound);
}

/// Every introspection query, so the tier table below covers the whole query surface.
const QUERIES: [QueryKind; 4] = [
    QueryKind::Describe,
    QueryKind::Neighbors,
    QueryKind::History,
    QueryKind::Why,
];

/// Every agent tier, low to high.
const TIERS: [Tier; 4] = [Tier::T0, Tier::T1, Tier::T2, Tier::T3];

/// The query time every answer in the table is taken at.
const AT: i64 = 500;

/// One answer to one query: what the caller can read, plus the record the owner reads.
struct Answer {
    error: Option<SelfModelError>,
    record: QueryRecord,
}

impl Answer {
    /// The failure the caller can read, in its `Debug` form.
    fn debug(&self) -> String {
        format!("{:?}", self.error)
    }

    /// The failure the caller can read, in its `Display` form.
    fn display(&self) -> String {
        self.error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// The failure the caller can read, serialised as a host would emit it.
    fn wire(&self) -> String {
        match &self.error {
            Some(error) => toml::Value::try_from(error.to_owned())
                .expect("a self-model error serialises")
                .to_string(),
            None => String::new(),
        }
    }

    /// Whether the node was refused as absent.
    fn refused_as_absent(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.code() == spot_self_model::codes::NODE_NOT_FOUND)
    }
}

/// Runs one query for one caller tier over one snapshot, with a fresh audit sink.
fn run(model: &SelfModel, tier: Tier, query: QueryKind, node: &NodeRef) -> Answer {
    let mut audit = MemoryAudit::new();
    let caller = Caller::new("agent:probe", tier);
    let error = {
        let mut service = SelfModelService::new(model, &mut audit);
        match query {
            QueryKind::Describe => service.describe(&caller, node, AT).err(),
            QueryKind::Neighbors => service.neighbors(&caller, node, &[], AT).err(),
            QueryKind::History => service.history(&caller, node, &TimeRange::all(), AT).err(),
            QueryKind::Why => service.why(&caller, node, AT).err(),
        }
    };
    let record = audit
        .records()
        .first()
        .expect("every query records one audit entry")
        .clone();
    Answer { error, record }
}

/// Asserts the caller cannot tell the two answers apart on any surface it can read.
fn assert_same_answer(hidden: &Answer, absent: &Answer, what: &str) {
    assert_eq!(
        hidden.error, absent.error,
        "{what}: the error value differs"
    );
    assert_eq!(
        hidden.debug(),
        absent.debug(),
        "{what}: the `Debug` differs"
    );
    assert_eq!(
        hidden.display(),
        absent.display(),
        "{what}: the `Display` differs"
    );
    assert_eq!(
        hidden.wire(),
        absent.wire(),
        "{what}: the serialised form differs"
    );
    if hidden.error.is_some() {
        // A refusal has something to compare: the comparison above is not between two blanks.
        assert!(!hidden.display().is_empty(), "{what}: no text to compare");
        assert!(!hidden.wire().is_empty(), "{what}: nothing serialised");
        assert!(
            hidden.debug().contains("NodeNotFound"),
            "{what}: the answer is not the absent-node one"
        );
    }
}

#[test]
fn a_caller_below_the_floor_cannot_tell_a_hidden_node_from_an_absent_one() {
    // The same key in two snapshots: `with_setting` holds the node behind its tier floor,
    // `without` does not hold it at all. Both answer the identical question.
    let (with_setting, _) = model();
    let without = model_without_the_setting();
    let hidden = NodeRef::new(NodeKind::Setting, "job.retry_limit");
    let visible = NodeRef::new(NodeKind::Field, "sales_invoice.total");
    let floor = NodeKind::Setting.min_tier();
    assert_eq!(floor, Tier::T1, "the setting is T1 configuration");

    for query in QUERIES {
        // Below the floor: present-but-hidden and absent answer identically.
        let denied = run(&with_setting, Tier::T0, query, &hidden);
        let missing = run(&without, Tier::T0, query, &hidden);
        assert_same_answer(&denied, &missing, &format!("T0 {}", query.as_str()));
        assert!(
            denied.refused_as_absent(),
            "{}: a hidden node must answer as absent",
            query.as_str()
        );

        // The audit records the truth for the owner, and both refusals did the same work.
        assert_eq!(
            denied.record.outcome,
            QueryOutcome::Denied,
            "{}: the audit loses the tier truth",
            query.as_str()
        );
        assert_eq!(
            missing.record.outcome,
            QueryOutcome::NotFound,
            "{}: the audit loses the absence",
            query.as_str()
        );
        assert_eq!(denied.record.query, missing.record.query);
        assert_eq!(denied.record.caller, missing.record.caller);
        assert_eq!(denied.record.tier, missing.record.tier);
        assert_eq!(denied.record.at, missing.record.at);
        assert_eq!(denied.record.returned, 0);
        assert_eq!(missing.record.returned, 0);
        assert_eq!(denied.record.subject, missing.record.subject);
    }

    for tier in TIERS {
        for query in QUERIES {
            // A node the caller's tier reaches is answered in both snapshots, so the rule hides
            // nothing that the caller may see.
            let allowed = run(&with_setting, tier, query, &visible);
            assert!(
                !allowed.refused_as_absent(),
                "{tier:?} {} answers for a T0 field",
                query.as_str()
            );

            let gone = run(&without, tier, query, &hidden);
            assert!(
                gone.refused_as_absent(),
                "{tier:?} {} reports the absent node",
                query.as_str()
            );

            if tier >= floor {
                // At or above the floor the same node is answered precisely, so the two are
                // still distinguished for a caller that may see it.
                let precise = run(&with_setting, tier, query, &hidden);
                assert!(
                    !precise.refused_as_absent(),
                    "{tier:?} {} answers for a setting it may see",
                    query.as_str()
                );
                assert_ne!(
                    precise.record.outcome,
                    QueryOutcome::Denied,
                    "{tier:?} {} is not denied a node it may see",
                    query.as_str()
                );
            } else {
                // Below the floor the node is answered exactly like the absent one.
                assert_same_answer(
                    &run(&with_setting, tier, query, &hidden),
                    &gone,
                    &format!("{tier:?} {}", query.as_str()),
                );
            }
        }
    }
}

#[test]
fn describe_neighbors_and_history_report_a_missing_node() {
    let (model, mut audit) = model();
    let caller = Caller::new("agent:t0", Tier::T0);
    let missing = NodeRef::new(NodeKind::Job, "ghost");
    let mut service = SelfModelService::new(&model, &mut audit);

    for error in [
        service
            .describe(&caller, &missing, 1)
            .expect_err("describe misses"),
        service
            .neighbors(&caller, &missing, &[], 1)
            .expect_err("neighbors misses"),
        service
            .history(&caller, &missing, &TimeRange::all(), 1)
            .expect_err("history misses"),
    ] {
        assert_eq!(error.code(), spot_self_model::codes::NODE_NOT_FOUND);
    }
    assert!(
        audit
            .records()
            .iter()
            .all(|record| record.outcome == QueryOutcome::NotFound)
    );
}
