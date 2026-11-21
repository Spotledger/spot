//! Versioning tests (brief §4): content hash invariants, the registry's
//! version chain (1,2,3 in order, `VersionGap`, `NoChange`, a held v1
//! surviving v2) and the JSON round trip with its rejection rules.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used)]

use spot_dag::{
    CompiledDag, ContentHash, DagDefinition, DagRegistry, Edge, JoinRule, Limits, Node, NodeId,
    RegistryError,
};

/// A node with default join (`All`) and empty config.
fn n(id: &str) -> Node {
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::All,
        config: serde_json::Value::Null,
    }
}

/// A node with a config value.
fn n_cfg(id: &str, config: serde_json::Value) -> Node {
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::All,
        config,
    }
}

/// An unconditional edge.
fn e(from: &str, to: &str) -> Edge {
    Edge {
        from: NodeId::new(from).unwrap(),
        to: NodeId::new(to).unwrap(),
        condition: None,
    }
}

/// An edge with a condition.
fn ec(from: &str, to: &str, condition: &str) -> Edge {
    Edge {
        from: NodeId::new(from).unwrap(),
        to: NodeId::new(to).unwrap(),
        condition: Some(condition.to_owned()),
    }
}

/// The plain diamond used across the hash tests.
fn diamond() -> DagDefinition {
    DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c"), n("d")],
        vec![e("a", "b"), e("a", "c"), e("b", "d"), e("c", "d")],
    )
}

mod hash {
    use super::*;

    #[test]
    fn display_is_64_lowercase_hex() {
        let hash = ContentHash::of(&diamond());
        let hex = hash.as_hex();
        assert_eq!(hex.len(), 64, "{hex}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{hex}"
        );
        assert_eq!(hash.to_string(), hex);
    }

    #[test]
    fn order_independent_for_nodes_and_edges() {
        // Same graph as diamond(), scrambled listing order.
        let twin = DagDefinition::new(
            "t",
            1,
            vec![n("d"), n("b"), n("c"), n("a")],
            vec![e("c", "d"), e("a", "c"), e("b", "d"), e("a", "b")],
        );
        assert_eq!(ContentHash::of(&diamond()), ContentHash::of(&twin));
    }

    #[test]
    fn config_key_order_independent() {
        let left = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({"alpha": 1, "beta": 2}))],
            vec![],
        );
        let right = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({"beta": 2, "alpha": 1}))],
            vec![],
        );
        assert_eq!(ContentHash::of(&left), ContentHash::of(&right));
    }

    #[test]
    fn nested_config_key_order_independent() {
        let left = DagDefinition::new(
            "t",
            1,
            vec![n_cfg(
                "a",
                serde_json::json!({"rows": [{"x": 1, "y": 2}], "flag": true}),
            )],
            vec![],
        );
        let right = DagDefinition::new(
            "t",
            1,
            vec![n_cfg(
                "a",
                serde_json::json!({"flag": true, "rows": [{"y": 2, "x": 1}]}),
            )],
            vec![],
        );
        assert_eq!(ContentHash::of(&left), ContentHash::of(&right));
    }

    #[test]
    fn version_is_not_hashed() {
        let mut bigger = diamond();
        bigger.version = 99;
        assert_eq!(ContentHash::of(&diamond()), ContentHash::of(&bigger));
    }

    #[test]
    fn real_edits_change_the_hash() {
        let base = diamond();
        let base_hash = ContentHash::of(&base);

        // A node kind edit.
        let mut kind_edit = diamond();
        kind_edit.nodes[1].kind = "gate".to_owned();
        assert_ne!(ContentHash::of(&kind_edit), base_hash);

        // An edge condition edit (None -> Some).
        let mut cond_edit = diamond();
        cond_edit.edges[0] = ec("a", "b", "approved");
        assert_ne!(ContentHash::of(&cond_edit), base_hash);

        // A config value edit.
        let mut cfg_edit = diamond();
        cfg_edit.nodes[0] = n_cfg("a", serde_json::json!({"weight": 3}));
        assert_ne!(ContentHash::of(&cfg_edit), base_hash);

        // A node id edit.
        let mut id_edit = diamond();
        id_edit.nodes[3].id = NodeId::new("end").unwrap();
        assert_ne!(ContentHash::of(&id_edit), base_hash);

        // Removing an edge.
        let mut drop_edit = diamond();
        drop_edit.edges.pop();
        assert_ne!(ContentHash::of(&drop_edit), base_hash);
    }
}

mod registry {
    use super::*;

    /// v1 / v2 / v3 of one key: each a real content edit.
    fn v1() -> DagDefinition {
        diamond()
    }

    fn v2() -> DagDefinition {
        let mut def = diamond();
        def.version = 2;
        def.nodes[1].kind = "gate".to_owned();
        def
    }

    fn v3() -> DagDefinition {
        let mut def = diamond();
        def.version = 3;
        def.nodes[1].kind = "checkpoint".to_owned();
        def
    }

    #[test]
    fn publishes_one_two_three_in_order() {
        let mut registry = DagRegistry::new();
        let p1 = registry.publish(&v1(), &Limits::new()).unwrap();
        assert_eq!(p1.version, 1);
        let p2 = registry.publish(&v2(), &Limits::new()).unwrap();
        assert_eq!(p2.version, 2);
        let p3 = registry.publish(&v3(), &Limits::new()).unwrap();
        assert_eq!(p3.version, 3);
        assert_eq!(registry.versions("t"), vec![1, 2, 3]);
        assert_eq!(registry.latest("t").unwrap().version(), 3);
        // Every version stays available and keeps its own content.
        assert_eq!(registry.get("t", 1).unwrap().nodes()[1].kind, "step");
        assert_eq!(registry.get("t", 2).unwrap().nodes()[1].kind, "gate");
        assert_eq!(registry.get("t", 3).unwrap().nodes()[1].kind, "checkpoint");
        // Published hashes carry through.
        assert_eq!(p2.hash, registry.get("t", 2).unwrap().hash().clone());
    }

    #[test]
    fn version_gap_after_first() {
        let mut registry = DagRegistry::new();
        registry.publish(&v1(), &Limits::new()).unwrap();
        let err = registry.publish(&v3(), &Limits::new()).unwrap_err();
        assert!(
            matches!(
                err,
                RegistryError::VersionGap {
                    expected: 2,
                    got: 3
                }
            ),
            "{err:?}"
        );
        // Nothing was stored.
        assert_eq!(registry.versions("t"), vec![1]);
    }

    #[test]
    fn first_version_must_be_one() {
        let mut registry = DagRegistry::new();
        let mut first = diamond();
        first.version = 2;
        let err = registry.publish(&first, &Limits::new()).unwrap_err();
        assert!(
            matches!(
                err,
                RegistryError::VersionGap {
                    expected: 1,
                    got: 2
                }
            ),
            "{err:?}"
        );
        assert!(registry.versions("t").is_empty());
    }

    #[test]
    fn identical_republish_is_no_change() {
        let mut registry = DagRegistry::new();
        registry.publish(&v1(), &Limits::new()).unwrap();
        // Same content, version bumped to what WOULD be next: rejected as
        // no-change and not stored.
        let mut again = diamond();
        again.version = 2;
        let err = registry.publish(&again, &Limits::new()).unwrap_err();
        assert!(
            matches!(err, RegistryError::NoChange { version: 2 }),
            "{err:?}"
        );
        assert_eq!(
            registry.versions("t"),
            vec![1],
            "the no-change version was not stored"
        );
        assert_eq!(registry.latest("t").unwrap().version(), 1);
    }

    #[test]
    fn validation_errors_come_back_as_invalid() {
        let mut registry = DagRegistry::new();
        // Version 0 fails validate() before the registry touches it.
        let mut bad = diamond();
        bad.version = 0;
        let err = registry.publish(&bad, &Limits::new()).unwrap_err();
        assert!(matches!(err, RegistryError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn held_v1_is_unchanged_after_v2() {
        // ADR-13 / brief §4: a running instance keeps its version. The held
        // handle is the immutable, Arc-shared `CompiledDag`.
        let mut registry = DagRegistry::new();
        registry.publish(&v1(), &Limits::new()).unwrap();
        let held: CompiledDag = registry.get("t", 1).unwrap().as_ref().clone();

        registry.publish(&v2(), &Limits::new()).unwrap();

        // The held v1 still reports v1 content and identity.
        assert_eq!(held.version(), 1);
        assert_eq!(held.nodes()[1].kind, "step");
        assert_eq!(held.hash().as_hex(), ContentHash::of(&v1()).as_hex());
        // It is not aliased to the newer version in the registry.
        assert_ne!(held.version(), registry.latest("t").unwrap().version());
        assert_ne!(
            held.hash().as_hex(),
            registry.latest("t").unwrap().hash().as_hex()
        );
    }
}

mod json {
    use super::*;

    #[test]
    fn to_json_from_json_round_trips() {
        let def = DagDefinition::new(
            "app.flow",
            4,
            vec![
                n_cfg("a", serde_json::json!({"weight": 2, "tags": ["x", "y"]})),
                Node {
                    join: JoinRule::Any,
                    ..n("b")
                },
                n("c"),
            ],
            vec![e("a", "b"), ec("a", "c", "approved"), e("b", "c")],
        );
        let text = def.to_json().unwrap();
        let parsed = DagDefinition::from_json(&text).unwrap();
        assert_eq!(parsed, def);
        // Round trip through a compacted form too.
        let reparsed = DagDefinition::from_json(&text).unwrap().to_json().unwrap();
        let twice = DagDefinition::from_json(&reparsed).unwrap();
        assert_eq!(twice, def);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = r#"{
            "key": "t",
            "version": 1,
            "nodes": [{"id": "a", "kind": "step"}],
            "edges": [],
            "surprise": true
        }"#;
        let err = DagDefinition::from_json(text).unwrap_err();
        assert!(matches!(err, spot_dag::DagError::Json { .. }), "{err:?}");
    }

    #[test]
    fn unknown_node_field_is_rejected() {
        let text = r#"{
            "key": "t",
            "version": 1,
            "nodes": [{"id": "a", "kind": "step", "weight": 3}],
            "edges": []
        }"#;
        assert!(DagDefinition::from_json(text).is_err());
    }

    #[test]
    fn bad_node_id_in_json_is_rejected() {
        // Uppercase breaks the NodeId grammar.
        let text = r#"{
            "key": "t",
            "version": 1,
            "nodes": [{"id": "Bad", "kind": "step"}],
            "edges": []
        }"#;
        assert!(DagDefinition::from_json(text).is_err());

        // Same grammar rule for an edge endpoint.
        let text = r#"{
            "key": "t",
            "version": 1,
            "nodes": [{"id": "a", "kind": "step"}],
            "edges": [{"from": "a", "to": "1bad"}]
        }"#;
        assert!(DagDefinition::from_json(text).is_err());
    }

    #[test]
    fn float_in_config_json_is_rejected() {
        let text = r#"{
            "key": "t",
            "version": 1,
            "nodes": [{"id": "a", "kind": "step", "config": {"price": 1.5}}],
            "edges": []
        }"#;
        assert!(DagDefinition::from_json(text).is_err());

        // Nested the same.
        let text = r#"{
            "key": "t",
            "version": 1,
            "nodes": [{"id": "a", "kind": "step", "config": {"xs": [1, 2.5]}}],
            "edges": []
        }"#;
        assert!(DagDefinition::from_json(text).is_err());
    }

    #[test]
    fn not_json_at_all_is_rejected() {
        assert!(DagDefinition::from_json("not json").is_err());
        assert!(DagDefinition::from_json("[]").is_err());
    }
}
