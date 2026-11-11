//! Hostile documents: control characters, a 10 KB name, and a
//! secret-looking value in a target and in an expression reach no problem
//! field, and a problem's text is built from its kind code and node id only.

// Test code may unwrap: every unwrap here is over a value the test itself built.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_model::validate::codes;
use spot_bpm_model::{
    DocumentAction, DocumentConfig, Edge, Limits, MetaView, Node, NodeId, NodeKind, Problem,
    Process, ProcessVersion, Rule, SubProcessConfig, TriggerConfig, TriggerType, VersionStatus,
    validate, validate_version,
};

const SECRET: &str = "sk_live_51HxQzSECRETVALUE";

/// A metadata view that knows nothing, so every target is unknown.
struct Nothing;

impl MetaView for Nothing {
    fn has_doctype(&self, _doctype: &str) -> bool {
        false
    }
    fn has_operation(&self, _doctype: &str, _operation: &str) -> bool {
        false
    }
    fn has_process(&self, _process: &str) -> bool {
        false
    }
    fn sub_processes(&self, _process: &str) -> Vec<String> {
        Vec::new()
    }
}

fn base() -> ProcessVersion {
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

/// Fragments no problem may carry.
fn needles() -> Vec<String> {
    vec![
        SECRET.to_owned(),
        "SECRETVALUE".to_owned(),
        "\u{1b}".to_owned(),
        "\u{7}".to_owned(),
        "\n".to_owned(),
        "AAAAAAAAAAAAAAAA".to_owned(),
    ]
}

fn hostile_name() -> String {
    format!(
        "{}\u{1b}[31m\u{7}\n{SECRET}{}",
        "A".repeat(10_240),
        "A".repeat(10_240)
    )
}

fn assert_clean(problems: &[Problem]) {
    assert!(!problems.is_empty());
    for problem in problems {
        let rendered = format!("{problem:?} {}", problem.message);
        for needle in needles() {
            assert!(
                !rendered.contains(&needle),
                "{needle:?} leaked into {rendered}"
            );
        }
        assert!(rendered.len() < 1_000, "problem text is bounded");
        assert!(
            !problem.message.is_empty(),
            "{}: message is meaningful",
            problem.code
        );
        assert!(problem.message.is_ascii());
        assert!(!problem.message.contains(char::is_control));
        assert_eq!(problem.message, problem.describe());
    }
}

#[test]
fn a_hostile_doctype_and_operation_leak_nothing() {
    let mut version = base();
    version.nodes[1].kind = NodeKind::Document(DocumentConfig {
        action: DocumentAction::Create,
        doctype: hostile_name(),
        operation: Some(hostile_name()),
        mapping: None,
        principal: None,
    });
    let problems = validate_version(&version, &Nothing, &Limits::new());
    assert!(problems.iter().any(|p| p.code == codes::UNKNOWN_DOCTYPE));
    assert_clean(&problems);
}

#[test]
fn a_hostile_subprocess_target_leaks_nothing() {
    let mut version = base();
    version.nodes[1].kind = NodeKind::SubProcess(SubProcessConfig {
        process: hostile_name(),
        version: None,
    });
    let problems = validate_version(&version, &Nothing, &Limits::new());
    assert!(problems.iter().any(|p| p.code == codes::SUBPROCESS_UNKNOWN));
    assert_clean(&problems);
}

#[test]
fn a_hostile_edge_rule_condition_and_rule_expression_leak_nothing() {
    let mut version = base();
    version.edges[0].rule = Some(hostile_name());
    let problems = validate_version(&version, &Nothing, &Limits::new());
    assert!(problems.iter().any(|p| p.code == codes::UNKNOWN_RULE));
    assert_clean(&problems);

    let mut version = base();
    version.edges[0].condition = Some(format!("(\n\u{1b}[31m \"{SECRET}\" {}", "A".repeat(10_240)));
    version.rules = vec![Rule::new(
        NodeId::new("r").unwrap(),
        format!("amount > \"{SECRET}\" ) \u{7}"),
    )];
    let problems = validate_version(&version, &Nothing, &Limits::new());
    let positioned: Vec<_> = problems
        .iter()
        .filter(|p| p.code == codes::EXPRESSION)
        .collect();
    assert_eq!(positioned.len(), 2);
    assert!(positioned.iter().all(|p| p.position.is_some()));
    assert_clean(&problems);
}

#[test]
fn a_hostile_process_name_and_app_leak_nothing() {
    let mut version = base();
    version.name = hostile_name();
    version.app = hostile_name();
    let process = Process {
        name: hostile_name(),
        app: hostile_name(),
        versions: vec![version],
        participants: None,
    };
    let problems = validate(&process, &Nothing, &Limits::new());
    assert!(problems.iter().any(|p| p.code == codes::INVALID_NAME));
    assert_clean(&problems);
}

#[test]
fn a_problem_is_described_from_its_code_and_node() {
    let mut version = base();
    version.edges[0].rule = Some("nope".to_owned());
    let problems = validate_version(&version, &Nothing, &Limits::new());
    let problem = problems
        .iter()
        .find(|p| p.code == codes::UNKNOWN_RULE)
        .unwrap();
    assert_eq!(problem.node.as_ref().map(NodeId::as_str), Some("start"));
    assert!(problem.message.contains("start"), "{}", problem.message);
    assert!(!problem.message.contains("nope"));
}
