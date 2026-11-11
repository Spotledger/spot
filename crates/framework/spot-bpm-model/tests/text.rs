//! The text form, the content hash and the run-state variables (brief §2, §4).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_model::{
    AllowAllMeta, CaseState, DocumentAction, DocumentConfig, Edge, Limits, LogicConfig, LogicType,
    Node, NodeId, NodeKind, ProcessVersion, RunStatus, TriggerConfig, TriggerType, VersionStatus,
    from_text, templates, to_text, validate_version,
};

/// A small valid version used across the tests.
fn sample() -> ProcessVersion {
    let start = Node::new(
        NodeId::new("start").unwrap(),
        NodeKind::Trigger(TriggerConfig {
            trigger: TriggerType::Manual,
            ..TriggerConfig::default()
        }),
    );
    let work = Node::new(
        NodeId::new("work").unwrap(),
        NodeKind::Document(DocumentConfig {
            action: DocumentAction::Create,
            doctype: "Sales Order".to_owned(),
            operation: None,
            mapping: None,
            principal: None,
        }),
    );
    let end = Node::new(
        NodeId::new("end").unwrap(),
        NodeKind::Logic(LogicConfig {
            logic: LogicType::Branch,
            ..LogicConfig::default()
        }),
    )
    .as_terminal();
    ProcessVersion {
        name: "demo".to_owned(),
        app: "demo_app".to_owned(),
        title: Some("Demo".to_owned()),
        version: 1,
        status: VersionStatus::Draft,
        nodes: vec![start, work, end],
        edges: vec![
            Edge::new(NodeId::new("start").unwrap(), NodeId::new("work").unwrap()),
            Edge::new(NodeId::new("work").unwrap(), NodeId::new("end").unwrap()),
        ],
        rules: vec![],
    }
}

#[test]
fn text_round_trips_and_is_idempotent() {
    let version = sample();
    let text = to_text(&version);
    let parsed = from_text(&text).unwrap();
    assert_eq!(parsed, version);
    assert_eq!(to_text(&parsed), text);
}

#[test]
fn identical_content_hashes_the_same_and_a_change_differs() {
    let a = sample();
    let mut b = sample();
    // Reorder nodes and edges: the hash must not move.
    b.nodes.reverse();
    b.edges.reverse();
    assert_eq!(a.content_hash(), b.content_hash());

    // A real change moves it.
    let mut c = sample();
    c.version = 2;
    assert_eq!(
        a.content_hash(),
        c.content_hash(),
        "the version number is not content"
    );
    c.status = VersionStatus::Published;
    assert_eq!(
        a.content_hash(),
        c.content_hash(),
        "the status is not content"
    );
    c.nodes[1].kind = NodeKind::Document(DocumentConfig {
        action: DocumentAction::Post,
        doctype: "Sales Order".to_owned(),
        operation: Some("post".to_owned()),
        mapping: None,
        principal: None,
    });
    assert_ne!(a.content_hash(), c.content_hash());
}

#[test]
fn bad_text_is_a_text_error_not_a_panic() {
    assert!(from_text("this is not toml = =").is_err());
    assert!(from_text("app = \"demo\"\nversion = 1\nstatus = \"draft\"\n").is_err());
    assert!(from_text("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"nope\"\n").is_err());
}

#[test]
fn bad_named_config_in_a_known_kind_is_rejected() {
    let text = r#"
name = "x"
app = "demo"
version = 1
status = "draft"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "document_event"
"#;
    // A document-event trigger needs a DocType and an operation, but the text
    // parses; validation reports it.
    let version = from_text(text).unwrap();
    let problems = validate_version(&version, &AllowAllMeta, &Limits::new());
    assert!(
        problems
            .iter()
            .any(|p| p.code == "bpm_model.trigger_incomplete")
    );
}

#[test]
fn case_state_variables_are_canonical_json() {
    let mut state = CaseState::new("demo", 1);
    state.variables = serde_json::json!({ "b": 2, "a": [1, 2] });
    state.status = RunStatus::Running;
    assert_eq!(state.canonical_variables(), r#"{"a":[1,2],"b":2}"#);
}

#[test]
fn no_user_visible_string_says_case() {
    for (_, text) in templates::ALL {
        assert!(
            !text.contains("Case"),
            "a template shows the word Case to a user"
        );
    }
    assert!(!spot_bpm_model::model::labels::RUN_SINGULAR_DEFAULT.contains("Case"));
    assert!(!spot_bpm_model::model::labels::RUN_PLURAL_DEFAULT.contains("Case"));
    // A few representative messages.
    let version = from_text(
        "name = \"x\"\napp = \"demo\"\nversion = 0\nstatus = \"draft\"\n[[nodes]]\nid = \"a\"\nkind = \"logic\"\nlogic = \"branch\"\nterminal = true\n",
    )
    .unwrap();
    let problems = validate_version(&version, &AllowAllMeta, &Limits::new());
    assert!(!problems.is_empty());
    for problem in problems {
        assert!(problem.code.starts_with("bpm_model."), "{}", problem.code);
        if let Some(node) = &problem.node {
            assert_eq!(node.as_str(), "a");
        }
        assert_eq!(problem.message, problem.describe());
        assert!(!problem.message.contains("Case"), "{}", problem.message);
    }
}
