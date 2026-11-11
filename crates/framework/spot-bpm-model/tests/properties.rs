//! Property tests (brief §6): random valid processes round-trip text and
//! compile; random single-edit mutations either stay valid or yield a specific
//! problem code.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;

use spot_bpm_model::{
    AllowAllMeta, CaseState, DocumentAction, DocumentConfig, Edge, IntegrationConfig, JoinRule,
    Limits, LogicConfig, LogicType, Node, NodeId, NodeKind, ProcessVersion, Rule, RunStatus,
    TriggerConfig, TriggerType, UnknownNode, VersionStatus, compile, from_text, to_text,
    validate_version,
};

/// A chain: a trigger, some middle steps, and a terminal.
fn chain(middle: &[u8]) -> ProcessVersion {
    let mut nodes = vec![Node::new(
        NodeId::new("start").unwrap(),
        NodeKind::Trigger(TriggerConfig {
            trigger: TriggerType::Manual,
            ..TriggerConfig::default()
        }),
    )];
    for (i, code) in middle.iter().enumerate() {
        let id = NodeId::new(&format!("m{i}")).unwrap();
        let kind = match code % 3 {
            0 => NodeKind::Document(DocumentConfig {
                action: DocumentAction::Create,
                doctype: "Sample".to_owned(),
                operation: None,
                mapping: None,
                principal: None,
            }),
            1 => NodeKind::Integration(IntegrationConfig {
                connector: "http".to_owned(),
                action: "post".to_owned(),
                endpoint: None,
            }),
            _ => NodeKind::Logic(LogicConfig {
                logic: LogicType::Branch,
                ..LogicConfig::default()
            }),
        };
        nodes.push(Node::new(id, kind));
    }
    let end = Node::new(
        NodeId::new("end").unwrap(),
        NodeKind::Logic(LogicConfig {
            logic: LogicType::Branch,
            ..LogicConfig::default()
        }),
    )
    .as_terminal();
    nodes.push(end);

    let mut edges = Vec::new();
    for pair in nodes.windows(2) {
        edges.push(Edge::new(pair[0].id.clone(), pair[1].id.clone()));
    }

    ProcessVersion {
        name: "prop".to_owned(),
        app: "demo".to_owned(),
        title: None,
        version: 1,
        status: VersionStatus::Draft,
        nodes,
        edges,
        rules: vec![],
    }
}

/// The base for mutation tests: a valid three-node chain.
fn base() -> ProcessVersion {
    chain(&[0])
}

proptest! {
    #[test]
    fn random_chains_round_trip_and_compile(middle in prop::collection::vec(0u8..3, 0..6)) {
        let version = chain(&middle);
        let text = to_text(&version);
        let parsed = from_text(&text).expect("the text form parses");
        prop_assert_eq!(&parsed, &version);
        prop_assert!(validate_version(&version, &AllowAllMeta, &Limits::new()).is_empty());
        prop_assert!(compile(&version).is_ok());
    }

    #[test]
    fn single_edit_mutations_stay_valid_or_report_a_code(index in 0usize..13) {
        let mut version = base();
        let expected = apply_mutation(index, &mut version);
        let problems = validate_version(&version, &AllowAllMeta, &Limits::new());
        match expected {
            Some(code) => prop_assert!(
                problems.iter().any(|p| p.code == code),
                "mutation {index} expected {code}, got {problems:#?}"
            ),
            None => prop_assert!(problems.is_empty(), "mutation {index} expected valid, got {problems:#?}"),
        }
    }
}

/// Applies one single-edit mutation; returns the code it must produce, or
/// `None` when the result must stay valid.
fn apply_mutation(index: usize, version: &mut ProcessVersion) -> Option<&'static str> {
    match index {
        0 => {
            version.version = 0;
            Some("bpm_model.version_zero")
        }
        1 => {
            version.nodes[0].kind = NodeKind::Document(DocumentConfig {
                action: DocumentAction::Create,
                doctype: "Sample".to_owned(),
                operation: None,
                mapping: None,
                principal: None,
            });
            Some("bpm_model.missing_trigger")
        }
        2 => {
            let duplicate = version.nodes[0].clone();
            version.nodes.push(duplicate);
            Some("bpm_model.duplicate_node")
        }
        3 => {
            version.edges.push(Edge::new(
                NodeId::new("m0").unwrap(),
                NodeId::new("ghost").unwrap(),
            ));
            Some("bpm_model.unknown_node")
        }
        4 => {
            version.nodes[1].kind = NodeKind::Unknown(UnknownNode {
                kind: "mystery".to_owned(),
                config: toml::Table::new(),
            });
            Some("bpm_model.unknown_node_kind")
        }
        5 => {
            version.edges.push(Edge::new(
                NodeId::new("start").unwrap(),
                NodeId::new("end").unwrap(),
            ));
            Some("bpm_model.ambiguous_branch")
        }
        6 => {
            let last = version.nodes.len() - 1;
            version.nodes[last].terminal = false;
            Some("bpm_model.dead_end")
        }
        7 => {
            version
                .rules
                .push(Rule::new(NodeId::new("bad").unwrap(), "("));
            Some("bpm_model.expression")
        }
        8 => {
            version.edges[1].rule = Some("nope".to_owned());
            Some("bpm_model.unknown_rule")
        }
        9 => {
            version.nodes[1].join = JoinRule::Quorum { required: 5 };
            Some("bpm_model.join_insufficient")
        }
        10 => {
            version.edges.push(Edge::new(
                NodeId::new("end").unwrap(),
                NodeId::new("m0").unwrap(),
            ));
            Some("bpm_model.cycle")
        }
        11 => {
            version.nodes.clear();
            version.edges.clear();
            Some("bpm_model.empty")
        }
        _ => {
            version.title = Some("Renamed".to_owned());
            None
        }
    }
}

/// The run state starts empty and running, and its variables canonicalise.
#[test]
fn case_state_defaults_are_empty_and_running() {
    let state = CaseState::new("prop", 1);
    assert_eq!(state.status, RunStatus::Running);
    assert!(state.current.is_empty());
    assert_eq!(state.canonical_variables(), "{}");
}
