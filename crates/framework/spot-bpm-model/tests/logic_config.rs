//! Typed Loop/Map configuration (WP-BPM-01.T2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_bpm_model::{
    AllowAllMeta, Edge, Limits, LogicConfig, LogicType, MAX_LOOP_ITERATIONS, Node, NodeId,
    NodeKind, ProcessVersion, TriggerConfig, TriggerType, VersionStatus, codes, from_text, to_text,
    validate_version,
};

fn version_with(kind: NodeKind) -> ProcessVersion {
    ProcessVersion {
        name: "demo".to_owned(),
        app: "demo_app".to_owned(),
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
            Node::new(NodeId::new("work").unwrap(), kind),
            Node::new(
                NodeId::new("end").unwrap(),
                NodeKind::Logic(LogicConfig {
                    logic: LogicType::Branch,
                    ..LogicConfig::default()
                }),
            )
            .as_terminal(),
        ],
        edges: vec![
            Edge::new(NodeId::new("start").unwrap(), NodeId::new("work").unwrap()),
            Edge::new(NodeId::new("work").unwrap(), NodeId::new("end").unwrap()),
        ],
        rules: vec![],
    }
}

fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn loop_config() -> LogicConfig {
    LogicConfig {
        logic: LogicType::Loop,
        max_iterations: Some(5),
        condition: Some("doc.count < 3".to_owned()),
        body: pairs(&[("count", "doc.count + 1")]),
        branches: vec!["done".to_owned()],
        ..LogicConfig::default()
    }
}

fn map_config() -> LogicConfig {
    LogicConfig {
        logic: LogicType::Map,
        mapping: pairs(&[("total", "doc.a + doc.b")]),
        ..LogicConfig::default()
    }
}

fn problem_codes(config: LogicConfig) -> Vec<&'static str> {
    let v = version_with(NodeKind::Logic(config));
    validate_version(&v, &AllowAllMeta, &Limits::default())
        .iter()
        .map(|p| p.code)
        .collect()
}

#[test]
fn golden_definition_without_new_fields_keeps_its_hash() {
    // Captured before the typed fields existed: it must never move.
    let v = version_with(NodeKind::Logic(LogicConfig {
        logic: LogicType::Map,
        ..LogicConfig::default()
    }));
    assert_eq!(
        v.content_hash().as_hex(),
        "36911072b79522bad8f22955fb5ec60c955731c8edbff21ba38108d116bae617"
    );
}

#[test]
fn loop_and_map_round_trip_through_text() {
    for config in [loop_config(), map_config()] {
        let v = version_with(NodeKind::Logic(config));
        let text = to_text(&v);
        let parsed = from_text(&text).unwrap();
        assert_eq!(parsed, v);
        assert_eq!(to_text(&parsed), text);
        assert_eq!(parsed.content_hash(), v.content_hash());
    }
}

#[test]
fn text_with_loop_keys_is_accepted_and_valid() {
    let v = version_with(NodeKind::Logic(loop_config()));
    let text = to_text(&v);
    assert!(text.contains("max_iterations"));
    let parsed = from_text(&text).unwrap();
    assert!(validate_version(&parsed, &AllowAllMeta, &Limits::default()).is_empty());
}

#[test]
fn valid_loop_and_map_have_no_problems() {
    assert!(problem_codes(loop_config()).is_empty());
    assert!(problem_codes(map_config()).is_empty());
}

#[test]
fn keys_on_the_wrong_logic_type_are_reported() {
    let mut c = map_config();
    c.max_iterations = Some(2);
    assert_eq!(problem_codes(c), vec![codes::LOGIC_CONFIG_UNEXPECTED]);

    let mut c = loop_config();
    c.mapping = pairs(&[("x", "1")]);
    assert_eq!(problem_codes(c), vec![codes::LOGIC_CONFIG_UNEXPECTED]);

    let c = LogicConfig {
        logic: LogicType::Join,
        branches: vec!["a".to_owned()],
        ..LogicConfig::default()
    };
    assert_eq!(problem_codes(c), vec![codes::LOGIC_CONFIG_UNEXPECTED]);
}

#[test]
fn max_iterations_is_bounded() {
    for bad in [0, MAX_LOOP_ITERATIONS + 1] {
        let mut c = loop_config();
        c.max_iterations = Some(bad);
        assert_eq!(problem_codes(c), vec![codes::LOGIC_CONFIG_INVALID]);
    }
    for good in [1, MAX_LOOP_ITERATIONS] {
        let mut c = loop_config();
        c.max_iterations = Some(good);
        assert!(problem_codes(c).is_empty());
    }
}

#[test]
fn expressions_must_be_present_and_parse() {
    let mut c = loop_config();
    c.condition = Some("  ".to_owned());
    assert_eq!(problem_codes(c), vec![codes::LOGIC_CONFIG_INVALID]);

    let mut c = loop_config();
    c.condition = Some("doc.count <".to_owned());
    assert_eq!(problem_codes(c), vec![codes::EXPRESSION]);

    let mut c = map_config();
    c.mapping = pairs(&[("total", "doc.a +")]);
    assert_eq!(problem_codes(c), vec![codes::EXPRESSION]);
}

#[test]
fn model_cap_matches_the_engine_cap() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spot-bpm-engine/src/logic.rs"
    ))
    .unwrap();
    assert!(
        source.contains(".min(10_000)") && MAX_LOOP_ITERATIONS == 10_000,
        "the engine's loop cap differs from MAX_LOOP_ITERATIONS"
    );
}
