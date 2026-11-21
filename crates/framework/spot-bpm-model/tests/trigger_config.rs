//! The optional trigger fields: roles, input schema, condition, zone and
//! missed-run policy (WP-BPM-01.T5).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_bpm_model::{
    AllowAllMeta, Edge, Limits, LogicConfig, LogicType, MissedPolicy, Node, NodeId, NodeKind,
    Problem, ProcessVersion, TriggerConfig, TriggerType, VersionStatus, codes, compile, from_text,
    to_text, validate_version,
};

/// A three-line process text whose trigger table is `trigger_lines`.
fn text(trigger_lines: &str) -> String {
    format!(
        r#"name = "demo"
app = "demo_app"
version = 1
status = "draft"

[[nodes]]
id = "start"
kind = "trigger"
{trigger_lines}

[[nodes]]
id = "end"
kind = "logic"
logic = "branch"
terminal = true

[[edges]]
from = "start"
to = "end"
"#
    )
}

fn trigger(version: &ProcessVersion) -> TriggerConfig {
    version
        .nodes
        .iter()
        .find_map(|n| match &n.kind {
            NodeKind::Trigger(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap()
}

fn problems(version: &ProcessVersion) -> Vec<Problem> {
    validate_version(version, &AllowAllMeta, &Limits::new())
}

/// The problems with one of the two new codes, for the text `lines`.
fn field_problems(lines: &str) -> Vec<Problem> {
    problems(&from_text(&text(lines)).unwrap())
        .into_iter()
        .filter(|p| {
            p.code == codes::TRIGGER_FIELD_INVALID || p.code == codes::TRIGGER_FIELD_MISPLACED
        })
        .collect()
}

fn one(lines: &str, code: &str, secret: &str) {
    let found = field_problems(lines);
    assert_eq!(found.len(), 1, "{lines}: {found:?}");
    assert_eq!(found[0].code, code, "{lines}");
    assert_eq!(found[0].node.as_ref().unwrap().as_str(), "start");
    let shown = format!("{:?} {}", found[0], found[0].describe());
    assert!(!secret.is_empty() && !shown.contains(secret), "{shown}");
}

#[test]
fn manual_fields_parse_and_round_trip() {
    let lines = r#"trigger = "manual"
roles = ["approver", "clerk"]
input_schema = "{\"type\":\"object\",\"properties\":{\"qty\":{\"type\":\"integer\"},\"note\":{\"type\":[\"string\",\"null\"]}}}""#;
    let version = from_text(&text(lines)).unwrap();
    let config = trigger(&version);
    assert_eq!(config.roles, vec!["approver", "clerk"]);
    assert!(config.input_schema.as_deref().unwrap().contains("null"));
    assert!(field_problems(lines).is_empty());
    let again = from_text(&to_text(&version)).unwrap();
    assert_eq!(again, version);
    assert_eq!(again.content_hash(), version.content_hash());
}

#[test]
fn document_event_condition_parses_and_round_trips() {
    let lines = r#"trigger = "document_event"
doctype = "Invoice"
event = "post"
condition = "doc.total > 100""#;
    let version = from_text(&text(lines)).unwrap();
    assert_eq!(
        trigger(&version).condition.as_deref(),
        Some("doc.total > 100")
    );
    assert!(field_problems(lines).is_empty());
    assert_eq!(from_text(&to_text(&version)).unwrap(), version);
}

#[test]
fn schedule_fields_parse_and_round_trip() {
    let lines = r#"trigger = "schedule"
cron = "0 9 * * *"
zone = "Asia/Karachi"
missed = "catch_up"
catch_up_max = 5"#;
    let version = from_text(&text(lines)).unwrap();
    let config = trigger(&version);
    assert_eq!(config.zone.as_deref(), Some("Asia/Karachi"));
    assert_eq!(config.missed, Some(MissedPolicy::CatchUp));
    assert_eq!(config.catch_up_max, Some(5));
    assert!(field_problems(lines).is_empty());
    assert_eq!(from_text(&to_text(&version)).unwrap(), version);
    for (word, policy) in [
        ("skip", MissedPolicy::Skip),
        ("run_once", MissedPolicy::RunOnce),
    ] {
        let l = format!("trigger = \"schedule\"\ncron = \"0 9 * * *\"\nmissed = \"{word}\"");
        let v = from_text(&text(&l)).unwrap();
        assert_eq!(trigger(&v).missed, Some(policy));
        assert!(field_problems(&l).is_empty());
    }
}

#[test]
fn text_without_the_new_fields_is_unchanged() {
    let version = from_text(&text("trigger = \"manual\"")).unwrap();
    let out = to_text(&version);
    for key in [
        "roles",
        "input_schema",
        "condition",
        "zone",
        "missed",
        "catch_up_max",
    ] {
        assert!(!out.contains(key), "{key} leaked into {out}");
    }
    assert_eq!(
        from_text(&out).unwrap().content_hash(),
        version.content_hash()
    );
    // Empty roles are the same as absent roles.
    let empty = from_text(&text("trigger = \"manual\"\nroles = []")).unwrap();
    assert_eq!(to_text(&empty), out);
    assert_eq!(empty.content_hash(), version.content_hash());
}

#[test]
fn compiled_node_config_carries_the_keys() {
    let lines = r#"trigger = "manual"
roles = ["clerk"]
input_schema = "{\"type\":\"object\"}""#;
    let compiled = compile(&from_text(&text(lines)).unwrap()).unwrap();
    let node = compiled
        .definition
        .nodes
        .iter()
        .find(|n| n.id.as_str() == "start")
        .unwrap();
    assert_eq!(node.config["roles"], serde_json::json!(["clerk"]));
    assert_eq!(node.config["input_schema"], "{\"type\":\"object\"}");

    let lines = r#"trigger = "schedule"
cron = "0 9 * * *"
zone = "UTC"
missed = "catch_up"
catch_up_max = 3"#;
    let compiled = compile(&from_text(&text(lines)).unwrap()).unwrap();
    let node = compiled
        .definition
        .nodes
        .iter()
        .find(|n| n.id.as_str() == "start")
        .unwrap();
    assert_eq!(node.config["zone"], "UTC");
    assert_eq!(node.config["missed"], "catch_up");
    assert_eq!(node.config["catch_up_max"], 3);

    let lines = "trigger = \"document_event\"\ndoctype = \"Invoice\"\nevent = \"post\"\ncondition = \"doc.total > 1\"";
    let compiled = compile(&from_text(&text(lines)).unwrap()).unwrap();
    let node = compiled
        .definition
        .nodes
        .iter()
        .find(|n| n.id.as_str() == "start")
        .unwrap();
    assert_eq!(node.config["condition"], "doc.total > 1");
}

#[test]
fn invalid_roles_are_one_problem_without_the_value() {
    let many: Vec<String> = (0..33).map(|i| format!("\"role_{i}\"")).collect();
    let long = "r".repeat(129);
    one(
        "trigger = \"manual\"\nroles = [\"has space\"]",
        codes::TRIGGER_FIELD_INVALID,
        "has space",
    );
    one(
        "trigger = \"manual\"\nroles = [\"\"]",
        codes::TRIGGER_FIELD_INVALID,
        "\"\"",
    );
    one(
        "trigger = \"manual\"\nroles = [\"dup_role\", \"dup_role\"]",
        codes::TRIGGER_FIELD_INVALID,
        "dup_role",
    );
    one(
        &format!("trigger = \"api\"\npath = \"p\"\nroles = [\"{long}\"]"),
        codes::TRIGGER_FIELD_INVALID,
        &long,
    );
    one(
        &format!("trigger = \"manual\"\nroles = [{}]", many.join(", ")),
        codes::TRIGGER_FIELD_INVALID,
        "role_7",
    );
    let ok: Vec<String> = (0..32).map(|i| format!("\"role_{i}\"")).collect();
    assert!(
        field_problems(&format!(
            "trigger = \"manual\"\nroles = [{}]",
            ok.join(", ")
        ))
        .is_empty()
    );
}

#[test]
fn invalid_input_schema_is_one_problem_without_the_value() {
    one(
        "trigger = \"manual\"\ninput_schema = \"not json secretword\"",
        codes::TRIGGER_FIELD_INVALID,
        "secretword",
    );
    one(
        "trigger = \"manual\"\ninput_schema = \"[1, 2]\"",
        codes::TRIGGER_FIELD_INVALID,
        "[1, 2]",
    );
    one(
        "trigger = \"manual\"\ninput_schema = \"{\\\"maximum\\\": 1.5}\"",
        codes::TRIGGER_FIELD_INVALID,
        "1.5",
    );
    one(
        "trigger = \"manual\"\ninput_schema = \"{\\\"maximum\\\": 1e3}\"",
        codes::TRIGGER_FIELD_INVALID,
        "1e3",
    );
    let deep = format!("{}1{}", "[".repeat(33), "]".repeat(33));
    one(
        &format!("trigger = \"manual\"\ninput_schema = \"{{\\\"a\\\": {deep}}}\""),
        codes::TRIGGER_FIELD_INVALID,
        &deep,
    );
    let big = format!("{{\\\"k\\\": \\\"{}\\\"}}", "x".repeat(17 * 1024));
    one(
        &format!("trigger = \"manual\"\ninput_schema = \"{big}\""),
        codes::TRIGGER_FIELD_INVALID,
        &"x".repeat(100),
    );
    // Integers and nesting within the limit are fine.
    assert!(
        field_problems("trigger = \"manual\"\ninput_schema = \"{\\\"maximum\\\": 10}\"").is_empty()
    );
}

#[test]
fn invalid_condition_is_one_problem_without_the_value() {
    let head = "trigger = \"document_event\"\ndoctype = \"Invoice\"\nevent = \"post\"\n";
    one(
        &format!("{head}condition = \"doc.total >> )(\""),
        codes::TRIGGER_FIELD_INVALID,
        "doc.total",
    );
    one(
        &format!("{head}condition = \"\""),
        codes::TRIGGER_FIELD_INVALID,
        "\"\"",
    );
    let long = format!("doc.a == \\\"{}\\\"", "y".repeat(2_000));
    one(
        &format!("{head}condition = \"{long}\""),
        codes::TRIGGER_FIELD_INVALID,
        &"y".repeat(100),
    );
}

#[test]
fn invalid_zone_and_missed_settings() {
    let head = "trigger = \"schedule\"\ncron = \"0 9 * * *\"\n";
    one(
        &format!("{head}zone = \"Asia Karachi\""),
        codes::TRIGGER_FIELD_INVALID,
        "Asia Karachi",
    );
    one(
        &format!("{head}zone = \"\""),
        codes::TRIGGER_FIELD_INVALID,
        "\"\"",
    );
    let long = "z".repeat(65);
    one(
        &format!("{head}zone = \"{long}\""),
        codes::TRIGGER_FIELD_INVALID,
        &long,
    );
    one(
        &format!("{head}missed = \"catch_up\""),
        codes::TRIGGER_FIELD_INVALID,
        "catch_up",
    );
    one(
        &format!("{head}missed = \"catch_up\"\ncatch_up_max = 0"),
        codes::TRIGGER_FIELD_INVALID,
        "catch_up_max",
    );
    one(
        &format!("{head}missed = \"catch_up\"\ncatch_up_max = 1001"),
        codes::TRIGGER_FIELD_INVALID,
        "1001",
    );
    one(
        &format!("{head}missed = \"skip\"\ncatch_up_max = 4"),
        codes::TRIGGER_FIELD_INVALID,
        "catch_up_max",
    );
    one(
        &format!("{head}catch_up_max = 4"),
        codes::TRIGGER_FIELD_INVALID,
        "catch_up_max",
    );
    assert!(
        field_problems(&format!("{head}missed = \"catch_up\"\ncatch_up_max = 1000")).is_empty()
    );
    assert!(field_problems(&format!("{head}zone = \"Etc/GMT+5\"")).is_empty());
}

#[test]
fn misplaced_fields_are_refused() {
    let doc = "trigger = \"document_event\"\ndoctype = \"Invoice\"\nevent = \"post\"\n";
    let sched = "trigger = \"schedule\"\ncron = \"0 9 * * *\"\n";
    let hook = "trigger = \"webhook\"\npath = \"p\"\n";
    let misplaced = codes::TRIGGER_FIELD_MISPLACED;
    one(
        "trigger = \"manual\"\ncondition = \"doc.a == 1\"",
        misplaced,
        "doc.a",
    );
    one("trigger = \"manual\"\nzone = \"UTC\"", misplaced, "UTC");
    one("trigger = \"manual\"\nmissed = \"skip\"", misplaced, "skip");
    one("trigger = \"manual\"\ncatch_up_max = 77", misplaced, "77");
    one(
        &format!("{doc}roles = [\"secret_role\"]"),
        misplaced,
        "secret_role",
    );
    one(&format!("{doc}input_schema = \"{{}}\""), misplaced, "{}");
    one(&format!("{doc}zone = \"UTC\""), misplaced, "UTC");
    one(
        &format!("{sched}roles = [\"secret_role\"]"),
        misplaced,
        "secret_role",
    );
    one(&format!("{sched}input_schema = \"{{}}\""), misplaced, "{}");
    one(
        &format!("{sched}condition = \"doc.a == 1\""),
        misplaced,
        "doc.a",
    );
    one(
        &format!("{hook}roles = [\"secret_role\"]"),
        misplaced,
        "secret_role",
    );
    one(
        &format!("{hook}condition = \"doc.a == 1\""),
        misplaced,
        "doc.a",
    );
    one(&format!("{hook}zone = \"UTC\""), misplaced, "UTC");
}

#[test]
fn an_unknown_key_on_a_trigger_is_still_refused() {
    assert!(from_text(&text("trigger = \"manual\"\nsecret = \"x\"")).is_err());
    assert!(from_text(&text("trigger = \"manual\"\nrole = [\"x\"]")).is_err());
}

fn base(config: TriggerConfig) -> ProcessVersion {
    ProcessVersion {
        name: "demo".to_owned(),
        app: "demo_app".to_owned(),
        title: None,
        version: 1,
        status: VersionStatus::Draft,
        nodes: vec![
            Node::new(NodeId::new("start").unwrap(), NodeKind::Trigger(config)),
            Node::new(
                NodeId::new("end").unwrap(),
                NodeKind::Logic(LogicConfig {
                    logic: LogicType::Branch,
                    ..LogicConfig::default()
                }),
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

fn valid_config() -> impl Strategy<Value = TriggerConfig> {
    let roles = prop::collection::btree_set("[a-z_]{1,12}", 0..5)
        .prop_map(|s| s.into_iter().collect::<Vec<String>>());
    let schema = prop::option::of(prop_oneof![
        Just("{\"type\":\"object\"}".to_owned()),
        Just("{\"type\":\"object\",\"properties\":{\"n\":{\"type\":\"integer\"}}}".to_owned()),
    ]);
    let manual = (roles, schema).prop_map(|(roles, input_schema)| TriggerConfig {
        trigger: TriggerType::Manual,
        roles,
        input_schema,
        ..TriggerConfig::default()
    });
    let doc = prop::option::of(prop_oneof![
        Just("doc.total > 1".to_owned()),
        Just("doc.status == \"open\"".to_owned()),
    ])
    .prop_map(|condition| TriggerConfig {
        trigger: TriggerType::DocumentEvent,
        doctype: Some("Invoice".to_owned()),
        event: Some("post".to_owned()),
        condition,
        ..TriggerConfig::default()
    });
    let sched = (
        prop::option::of(prop_oneof![
            Just("UTC".to_owned()),
            Just("Asia/Karachi".to_owned())
        ]),
        prop_oneof![
            Just((None, None)),
            Just((Some(MissedPolicy::Skip), None)),
            Just((Some(MissedPolicy::RunOnce), None)),
            (1u32..=1000).prop_map(|n| (Some(MissedPolicy::CatchUp), Some(n))),
        ],
    )
        .prop_map(|(zone, (missed, catch_up_max))| TriggerConfig {
            trigger: TriggerType::Schedule,
            cron: Some("0 9 * * *".to_owned()),
            zone,
            missed,
            catch_up_max,
            ..TriggerConfig::default()
        });
    prop_oneof![manual, doc, sched]
}

proptest! {
    #[test]
    fn valid_field_combinations_round_trip(config in valid_config()) {
        let version = base(config);
        prop_assert!(problems(&version).is_empty(), "{:?}", problems(&version));
        let parsed = from_text(&to_text(&version)).expect("the text parses");
        prop_assert_eq!(&parsed, &version);
        prop_assert_eq!(parsed.content_hash(), version.content_hash());
        prop_assert!(compile(&version).is_ok());
    }
}
