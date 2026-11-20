//! `TriggerSpec::from_config` reads the model's trigger fields (WP-BPM-03.T6).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use jiff::Timestamp;
use serde_json::json;
use spot_bpm_model::{
    Edge, MissedPolicy, Node, NodeId, NodeKind, ProcessVersion, Rule, TriggerConfig, TriggerType,
    VersionStatus,
};
use spot_bpm_triggers::{
    Error, SourceEvent, TriggerErrorCode, TriggerIndex, TriggerRegistry, TriggerSpec,
    validate_input,
};
use spot_events_contract::Topic;
use spot_permissions::Principal;
use spot_scheduler::MissedRunPolicy;
use spot_webhooks::{CommittedEvent, EventType};
use time::OffsetDateTime;

fn id(text: &str) -> NodeId {
    NodeId::new(text).unwrap()
}

fn spec(config: &TriggerConfig) -> Result<TriggerSpec, Error> {
    TriggerSpec::from_config("p", "app", &id("start"), config)
}

fn manual() -> TriggerConfig {
    TriggerConfig {
        trigger: TriggerType::Manual,
        ..TriggerConfig::default()
    }
}

fn api() -> TriggerConfig {
    TriggerConfig {
        trigger: TriggerType::Api,
        path: Some("start_order".into()),
        ..TriggerConfig::default()
    }
}

fn document() -> TriggerConfig {
    TriggerConfig {
        trigger: TriggerType::DocumentEvent,
        doctype: Some("sales_order".into()),
        event: Some("update".into()),
        ..TriggerConfig::default()
    }
}

fn schedule() -> TriggerConfig {
    TriggerConfig {
        trigger: TriggerType::Schedule,
        cron: Some("0 0 9 * * *".into()),
        ..TriggerConfig::default()
    }
}

const SCHEMA: &str =
    r#"{"type":"object","required":["qty"],"properties":{"qty":{"type":"integer"}}}"#;

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

/// Never echoes: neither the display text nor the debug text carries `secret`.
fn assert_no_echo(error: &Error, secret: &str) {
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains(secret), "{shown}");
}

#[test]
fn roles_and_schema_reach_the_manual_and_api_specs() {
    for config in [manual(), api()] {
        let config = TriggerConfig {
            roles: vec!["clerk".into(), "approver".into()],
            input_schema: Some(SCHEMA.into()),
            ..config
        };
        let built = spec(&config).unwrap();
        assert_eq!(built.roles(), ["clerk".to_owned(), "approver".to_owned()]);
        let schema = built.input_schema().unwrap();
        assert!(validate_input(schema, &json!({"qty": 2})).is_empty());
        assert_eq!(validate_input(schema, &json!({"qty": "two"})).len(), 1);
        assert_eq!(validate_input(schema, &json!({})).len(), 1);
    }
}

#[test]
fn a_condition_gates_a_document_event_through_the_matcher() {
    let config = TriggerConfig {
        condition: Some("doc.status == 'Done'".into()),
        ..document()
    };
    let built = spec(&config).unwrap();
    assert_eq!(
        built.as_document_event().unwrap().condition.as_deref(),
        Some("doc.status == 'Done'")
    );
    let version = ProcessVersion::new(
        "order_to_cash",
        "erp_selling",
        1,
        VersionStatus::Published,
        vec![Node::new(id("start"), NodeKind::Trigger(config))],
        Vec::<Edge>::new(),
        Vec::<Rule>::new(),
    )
    .unwrap();
    let index = TriggerIndex::from_versions(&[version]).unwrap();
    let registry = TriggerRegistry::with_builtin_kinds();
    let event = |status: &str| {
        SourceEvent::document(
            CommittedEvent {
                event_id: "evt-1".into(),
                topic: Topic::any(EventType::Update.topic_name()).unwrap(),
                doctype: "sales_order".into(),
                document_id: "doc-1".into(),
                document_version: 1,
                event_type: EventType::Update,
                occurred_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                after_image: json!({"status": status}),
                before_image: None,
            },
            Principal::system(),
        )
    };
    assert!(index.matching(&event("Open"), &registry).matches.is_empty());
    assert_eq!(index.matching(&event("Done"), &registry).matches.len(), 1);
}

#[test]
fn a_zone_shifts_the_schedule_by_its_offset() {
    let utc = spec(&schedule()).unwrap();
    let karachi = spec(&TriggerConfig {
        zone: Some("Asia/Karachi".into()),
        ..schedule()
    })
    .unwrap();
    let from = at("2026-01-01T00:00:00Z");
    let in_utc = utc
        .as_schedule()
        .unwrap()
        .schedule
        .next_after(from)
        .unwrap();
    let in_karachi = karachi
        .as_schedule()
        .unwrap()
        .schedule
        .next_after(from)
        .unwrap();
    assert_eq!(in_utc, at("2026-01-01T09:00:00Z"));
    assert_eq!(in_karachi, at("2026-01-01T04:00:00Z"));
    assert_eq!(in_utc.as_second() - in_karachi.as_second(), 5 * 3600);
}

#[test]
fn missed_policy_reaches_the_schedule_spec() {
    let policy = |config: TriggerConfig| spec(&config).unwrap().as_schedule().unwrap().policy;
    assert_eq!(policy(schedule()), MissedRunPolicy::RunOnce);
    assert_eq!(
        policy(TriggerConfig {
            missed: Some(MissedPolicy::RunOnce),
            ..schedule()
        }),
        MissedRunPolicy::RunOnce
    );
    assert_eq!(
        policy(TriggerConfig {
            missed: Some(MissedPolicy::CatchUp),
            catch_up_max: Some(3),
            ..schedule()
        }),
        MissedRunPolicy::CatchUp(3)
    );
}

#[test]
fn skip_is_refused_because_the_scheduler_would_never_fire() {
    let error = spec(&TriggerConfig {
        missed: Some(MissedPolicy::Skip),
        ..schedule()
    })
    .unwrap_err();
    assert_eq!(error.code(), TriggerErrorCode::ScheduleInvalid);
    let error = spec(&TriggerConfig {
        missed: Some(MissedPolicy::CatchUp),
        ..schedule()
    })
    .unwrap_err();
    assert_eq!(error.code(), TriggerErrorCode::ScheduleInvalid);
}

#[test]
fn unusable_values_are_refused_without_echoing_them() {
    let cases: Vec<(TriggerConfig, TriggerErrorCode, &str)> = vec![
        (
            TriggerConfig {
                input_schema: Some("{not json secretword".into()),
                ..manual()
            },
            TriggerErrorCode::InputSchemaInvalid,
            "secretword",
        ),
        (
            TriggerConfig {
                input_schema: Some("[\"secretword\"]".into()),
                ..api()
            },
            TriggerErrorCode::InputSchemaInvalid,
            "secretword",
        ),
        (
            TriggerConfig {
                input_schema: Some(r#"{"pattern":"^secretword"}"#.into()),
                ..manual()
            },
            TriggerErrorCode::InputSchemaInvalid,
            "secretword",
        ),
        (
            TriggerConfig {
                input_schema: Some(r#"{"properties":{"a":{"type":"secretword"}}}"#.into()),
                ..manual()
            },
            TriggerErrorCode::InputSchemaInvalid,
            "secretword",
        ),
        (
            TriggerConfig {
                condition: Some("doc.secretword ==".into()),
                ..document()
            },
            TriggerErrorCode::ConditionInvalid,
            "secretword",
        ),
        (
            TriggerConfig {
                zone: Some("Mars/Secretword".into()),
                ..schedule()
            },
            TriggerErrorCode::ScheduleInvalid,
            "Secretword",
        ),
    ];
    for (config, code, secret) in cases {
        let error = spec(&config).unwrap_err();
        assert_eq!(error.code(), code, "{config:?}");
        assert_no_echo(&error, secret);
    }
}

#[test]
fn a_condition_error_names_the_parse_code_and_offset() {
    let error = spec(&TriggerConfig {
        condition: Some("doc.status ==".into()),
        ..document()
    })
    .unwrap_err();
    match error {
        Error::ConditionInvalid { node, detail } => {
            assert_eq!(node, "start");
            assert!(
                detail.contains(" at offset ") || detail.contains("does not parse"),
                "{detail}"
            );
        }
        other => panic!("{other:?}"),
    }
}

fn published(nodes: Vec<Node>) -> ProcessVersion {
    ProcessVersion::new(
        "order_to_cash",
        "erp_selling",
        1,
        VersionStatus::Published,
        nodes,
        Vec::<Edge>::new(),
        Vec::<Rule>::new(),
    )
    .unwrap()
}

#[test]
fn a_version_with_three_bad_triggers_reports_all_three() {
    let version = published(vec![
        Node::new(
            id("bad_schema"),
            NodeKind::Trigger(TriggerConfig {
                input_schema: Some("nope".into()),
                ..manual()
            }),
        ),
        Node::new(
            id("bad_condition"),
            NodeKind::Trigger(TriggerConfig {
                condition: Some("doc.x ==".into()),
                ..document()
            }),
        ),
        Node::new(
            id("bad_zone"),
            NodeKind::Trigger(TriggerConfig {
                zone: Some("Nowhere/Land".into()),
                ..schedule()
            }),
        ),
        Node::new(id("fine"), NodeKind::Trigger(manual())),
    ]);
    let error = TriggerIndex::from_versions(&[version]).unwrap_err();
    assert_eq!(error.code(), TriggerErrorCode::TriggersInvalid);
    let Error::TriggersInvalid { process, faults } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(process, "order_to_cash");
    let listed: Vec<(&str, TriggerErrorCode)> =
        faults.iter().map(|f| (f.node.as_str(), f.code)).collect();
    assert_eq!(
        listed,
        vec![
            ("bad_schema", TriggerErrorCode::InputSchemaInvalid),
            ("bad_condition", TriggerErrorCode::ConditionInvalid),
            ("bad_zone", TriggerErrorCode::ScheduleInvalid),
        ]
    );
    assert_no_echo(&error, "Nowhere");
}

#[test]
fn one_bad_trigger_keeps_its_own_error_and_adds_nothing() {
    let version = published(vec![
        Node::new(id("fine"), NodeKind::Trigger(manual())),
        Node::new(
            id("bad_schema"),
            NodeKind::Trigger(TriggerConfig {
                input_schema: Some("nope".into()),
                ..manual()
            }),
        ),
    ]);
    let mut index = TriggerIndex::new();
    let error = index.add_version(&version).unwrap_err();
    assert_eq!(error.code(), TriggerErrorCode::InputSchemaInvalid);
    assert!(index.is_empty());
}

#[test]
fn a_definition_without_the_new_fields_builds_the_old_defaults() {
    let m = spec(&manual()).unwrap();
    assert!(m.roles().is_empty());
    assert!(m.input_schema().is_none());
    let a = spec(&api()).unwrap();
    assert!(a.roles().is_empty());
    assert!(a.input_schema().is_none());
    assert_eq!(
        spec(&document())
            .unwrap()
            .as_document_event()
            .unwrap()
            .condition,
        None
    );
    let s = spec(&schedule()).unwrap();
    let s = s.as_schedule().unwrap();
    assert_eq!(s.policy, MissedRunPolicy::RunOnce);
    assert_eq!(
        s.schedule.next_after(at("2026-01-01T00:00:00Z")).unwrap(),
        at("2026-01-01T09:00:00Z"),
        "the default zone is UTC"
    );
    let hook = spec(&TriggerConfig {
        trigger: TriggerType::Webhook,
        path: Some("/hooks/x".into()),
        ..TriggerConfig::default()
    })
    .unwrap();
    assert_eq!(hook.as_webhook().unwrap().secret_ref, None);
}
