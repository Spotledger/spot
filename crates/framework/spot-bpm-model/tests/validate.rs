//! Validation: each problem the brief names is reported, and the codes are
//! stable and distinct.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use spot_bpm_model::validate::codes;
use spot_bpm_model::{
    CaseState, DocumentAction, DocumentConfig, DraftMode, Edge, FormConfig, Limits, MetaView, Node,
    NodeId, NodeKind, Participant, ParticipantKind, Participants, Process, ProcessVersion,
    RejectAction, SendMessageConfig, SubProcessConfig, TriggerConfig, TriggerType, UnknownNode,
    VersionStatus, validate, validate_version,
};

/// A metadata view the tests drive.
struct Meta {
    doctypes: Vec<&'static str>,
    operations: Vec<&'static str>,
    processes: BTreeMap<&'static str, Vec<&'static str>>,
}

impl Meta {
    fn empty() -> Self {
        Self {
            doctypes: vec![],
            operations: vec![],
            processes: BTreeMap::new(),
        }
    }

    fn with_doctype(mut self, doctype: &'static str) -> Self {
        self.doctypes.push(doctype);
        self
    }

    fn with_process(mut self, process: &'static str, calls: Vec<&'static str>) -> Self {
        self.processes.insert(process, calls);
        self
    }
}

impl MetaView for Meta {
    fn has_doctype(&self, doctype: &str) -> bool {
        self.doctypes.contains(&doctype)
    }

    fn has_operation(&self, _doctype: &str, operation: &str) -> bool {
        self.operations.contains(&operation)
    }

    fn has_process(&self, process: &str) -> bool {
        self.processes.contains_key(process) || process == "current"
    }

    fn sub_processes(&self, process: &str) -> Vec<String> {
        self.processes
            .get(process)
            .map(|calls| calls.iter().map(|c| (*c).to_owned()).collect())
            .unwrap_or_default()
    }
}

/// A valid two-node chain: a manual trigger into a terminal.
fn valid() -> ProcessVersion {
    ProcessVersion {
        name: "current".to_owned(),
        app: "demo".to_owned(),
        title: None,
        version: 1,
        status: VersionStatus::Draft,
        nodes: vec![
            Node::new(
                NodeId::new("start").unwrap(),
                NodeKind::Trigger(TriggerConfig {
                    trigger: TriggerType::Manual,
                    ..TriggerConfig::default()
                }),
            ),
            Node::new(
                NodeId::new("end").unwrap(),
                NodeKind::Logic(Default::default()),
            )
            .as_terminal(),
        ],
        edges: vec![Edge::new(
            NodeId::new("start").unwrap(),
            NodeId::new("end").unwrap(),
        )],
        rules: vec![],
    }
}

/// The codes a validation produced.
fn found(version: &ProcessVersion, meta: &dyn MetaView) -> BTreeSet<&'static str> {
    validate_version(version, meta, &Limits::new())
        .into_iter()
        .map(|p| p.code)
        .collect()
}

#[test]
fn a_valid_version_has_no_problems() {
    let meta = Meta::empty().with_doctype("Sample");
    assert!(validate_version(&valid(), &meta, &Limits::new()).is_empty());
}

#[test]
fn a_float_in_config_is_reported() {
    let mut version = valid();
    let mut table = toml::Table::new();
    table.insert("amount".to_owned(), toml::Value::Float(1.5));
    version.nodes[1].kind = NodeKind::Unknown(UnknownNode {
        kind: "custom".to_owned(),
        config: table,
    });
    let problems = found(&version, &Meta::empty());
    assert!(problems.contains(codes::FLOAT_IN_CONFIG), "{problems:?}");
    assert!(problems.contains(codes::UNKNOWN_NODE_KIND));
}

#[test]
fn an_unknown_doctype_is_reported() {
    let mut version = valid();
    version.nodes[1].kind = NodeKind::Document(DocumentConfig {
        action: DocumentAction::Create,
        doctype: "Nope".to_owned(),
        operation: None,
        mapping: None,
        principal: None,
    });
    let problems = found(&version, &Meta::empty());
    assert!(problems.contains(codes::UNKNOWN_DOCTYPE), "{problems:?}");
}

#[test]
fn an_unknown_operation_is_reported() {
    let mut version = valid();
    version.nodes[1].kind = NodeKind::Document(DocumentConfig {
        action: DocumentAction::Create,
        doctype: "Sample".to_owned(),
        operation: None,
        mapping: None,
        principal: None,
    });
    let meta = Meta::empty().with_doctype("Sample");
    let problems = found(&version, &meta);
    assert!(problems.contains(codes::UNKNOWN_OPERATION), "{problems:?}");
}

#[test]
fn a_sub_process_unknown_is_reported() {
    let mut version = valid();
    version.nodes[1].kind = NodeKind::SubProcess(SubProcessConfig {
        process: "missing".to_owned(),
        version: None,
    });
    let problems = found(&version, &Meta::empty());
    assert!(problems.contains(codes::SUBPROCESS_UNKNOWN), "{problems:?}");
}

#[test]
fn a_sub_process_cycle_is_reported() {
    let mut version = valid();
    version.nodes[1].kind = NodeKind::SubProcess(SubProcessConfig {
        process: "child".to_owned(),
        version: None,
    });
    let meta = Meta::empty().with_process("child", vec!["current"]);
    let problems = found(&version, &meta);
    assert!(problems.contains(codes::SUBPROCESS_CYCLE), "{problems:?}");
}

#[test]
fn an_island_is_unreachable_and_a_dead_end() {
    let mut version = valid();
    version.nodes.push(Node::new(
        NodeId::new("island").unwrap(),
        NodeKind::Logic(Default::default()),
    ));
    let problems = found(&version, &Meta::empty());
    assert!(problems.contains(codes::UNREACHABLE), "{problems:?}");
    assert!(problems.contains(codes::DEAD_END), "{problems:?}");
}

#[test]
fn the_depth_and_size_limits_are_reported() {
    let mut version = valid();
    version.nodes[1].terminal = false;
    for i in 0..4 {
        let id = NodeId::new(&format!("m{i}")).unwrap();
        version
            .nodes
            .push(Node::new(id, NodeKind::Logic(Default::default())));
    }
    version.nodes.last_mut().unwrap().terminal = true;
    version.edges.clear();
    for pair in version.nodes.windows(2) {
        version
            .edges
            .push(Edge::new(pair[0].id.clone(), pair[1].id.clone()));
    }
    let tight = Limits {
        max_nodes: 1,
        max_edges: 0,
        max_depth: 2,
    };
    let problems: BTreeSet<&str> = validate_version(&version, &Meta::empty(), &tight)
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(problems.contains(codes::TOO_MANY_NODES), "{problems:?}");
    assert!(problems.contains(codes::TOO_MANY_EDGES), "{problems:?}");
    assert!(problems.contains(codes::TOO_DEEP), "{problems:?}");
}

#[test]
fn a_process_mismatch_is_reported() {
    let version = valid();
    let process = Process::new("other", "demo", vec![version]).unwrap();
    let problems = validate(&process, &Meta::empty(), &Limits::new());
    assert!(
        problems.iter().any(|p| p.code == codes::PROCESS_MISMATCH),
        "{problems:#?}"
    );
}

#[test]
fn an_unknown_rule_and_a_bad_expression_are_reported() {
    let mut version = valid();
    version.edges[0].rule = Some("nope".to_owned());
    let problems = found(&version, &Meta::empty());
    assert!(problems.contains(codes::UNKNOWN_RULE), "{problems:?}");

    let mut version = valid();
    version.edges[0].condition = Some("(".to_owned());
    let problems = found(&version, &Meta::empty());
    assert!(problems.contains(codes::EXPRESSION), "{problems:?}");
}

#[test]
fn every_code_constant_is_distinct_and_prefixed() {
    let all = [
        codes::EMPTY,
        codes::VERSION_ZERO,
        codes::INVALID_NAME,
        codes::INVALID_KIND,
        codes::FLOAT_IN_CONFIG,
        codes::DUPLICATE_NODE,
        codes::DUPLICATE_EDGE,
        codes::DUPLICATE_RULE,
        codes::UNKNOWN_NODE,
        codes::UNKNOWN_NODE_KIND,
        codes::UNKNOWN_RULE,
        codes::CYCLE,
        codes::MISSING_TRIGGER,
        codes::UNREACHABLE,
        codes::DEAD_END,
        codes::AMBIGUOUS_BRANCH,
        codes::JOIN_INSUFFICIENT,
        codes::UNKNOWN_DOCTYPE,
        codes::UNKNOWN_OPERATION,
        codes::TRIGGER_INCOMPLETE,
        codes::EXPRESSION,
        codes::SUBPROCESS_UNKNOWN,
        codes::SUBPROCESS_CYCLE,
        codes::TOO_MANY_NODES,
        codes::TOO_MANY_EDGES,
        codes::TOO_DEEP,
        codes::PROCESS_MISMATCH,
        codes::DAG_INVALID,
    ];
    let unique: BTreeSet<&str> = all.iter().copied().collect();
    assert_eq!(unique.len(), all.len(), "two codes are the same");
    for code in all {
        assert!(code.starts_with("bpm_model."), "{code}");
    }
}

/// A run keeps its own variables and status.
#[test]
fn case_state_carries_variables_and_status() {
    let mut state = CaseState::new("current", 1);
    state.current.insert(NodeId::new("start").unwrap());
    assert!(!state.canonical_variables().is_empty());
}

/// A trigger into a form step that ends the flow.
fn form_version() -> ProcessVersion {
    let mut version = valid();
    version.nodes[1].kind = NodeKind::Form(FormConfig {
        record_type: Some("Sample".to_owned()),
        fields: vec!["amount".to_owned(), "reason".to_owned()],
        assignee: Some("role:clerk".to_owned()),
        draft_mode: DraftMode::Create,
    });
    version
}

#[test]
fn a_form_naming_an_installed_record_type_is_valid() {
    let meta = Meta::empty().with_doctype("Sample");
    assert!(
        validate_version(&form_version(), &meta, &Limits::new()).is_empty(),
        "{:#?}",
        validate_version(&form_version(), &meta, &Limits::new())
    );
}

#[test]
fn a_form_naming_an_unknown_record_type_is_reported() {
    let mut version = form_version();
    if let NodeKind::Form(config) = &mut version.nodes[1].kind {
        config.record_type = Some("Missing".to_owned());
    }
    assert!(found(&version, &Meta::empty()).contains(codes::UNKNOWN_DOCTYPE));
}

#[test]
fn a_form_that_collects_without_a_record_type_is_valid() {
    let mut version = form_version();
    if let NodeKind::Form(config) = &mut version.nodes[1].kind {
        config.record_type = None;
        config.draft_mode = DraftMode::Collect;
    }
    assert!(validate_version(&version, &Meta::empty(), &Limits::new()).is_empty());
}

#[test]
fn message_steps_are_validated_by_the_contract_checker() {
    // The model knows the kinds and checks their structure (edges, reachability,
    // expressions, floats); the contract semantics — which messages exist, the
    // order and the timeout branch — belong to `spot-bpm-contract`'s conformance
    // check, whose `bpm_contract.*` codes this crate cannot raise.
    let mut version = valid();
    version.nodes[1].kind = NodeKind::SendMessage(SendMessageConfig {
        contract: "approval".to_owned(),
        message: "request".to_owned(),
        mapping: None,
        on_reject: RejectAction::ErrorBranch,
    });
    version.nodes[1].terminal = true;
    version.edges.clear();
    // No error branch and no outgoing edge: only a dead end is the model's
    // problem here.
    let problems = found(&version, &Meta::empty());
    assert!(!problems.contains(codes::UNKNOWN_NODE_KIND), "{problems:?}");
}

#[test]
fn a_process_carries_its_participants() {
    let process = Process::new("current", "demo", vec![valid()])
        .unwrap()
        .with_participants(Participants::new(
            vec![
                Participant::new("requester", ParticipantKind::Tenant),
                Participant::new("approver", ParticipantKind::External),
            ],
            "requester",
        ));
    assert_eq!(
        process
            .participants
            .as_ref()
            .and_then(|participants| participants.participant("approver"))
            .map(|participant| participant.kind),
        Some(ParticipantKind::External)
    );
    // The model does not judge participants on its own; `conforms_process`
    // checks them against the contract they belong to.
    assert!(validate(&process, &Meta::empty(), &Limits::new()).is_empty());
}
