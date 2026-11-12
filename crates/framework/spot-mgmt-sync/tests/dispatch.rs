//! `dispatch`: one method per operation, typed arguments, refusals that fail closed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};
use spot_mgmt_sync::{ExecKind, OpError, dispatch, ids};
use support::*;

/// A hand-built `Validated`, as a forged or stale one would arrive (bypassing the catalogue).
fn forged(op: &str, params: &[(&str, ParamValue)]) -> Validated {
    Validated {
        op: OpId::new(op).unwrap(),
        params: params
            .iter()
            .map(|(name, value)| AcceptedParam {
                name: (*name).to_owned(),
                ty: ParamType::Text {
                    max_len: 1000,
                    pattern: None,
                },
                value: value.clone(),
                from_default: false,
                secret: false,
            })
            .collect(),
    }
}

fn refused(result: Result<serde_json::Value, OpError>) {
    assert!(
        matches!(result, Err(OpError::BadArguments { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn every_sample_reaches_exactly_its_method_and_returns_json() {
    let catalogue = catalogue();
    let expected = [
        "health",
        "node_list",
        "node_show branch07.spotledger.app",
        "node_enrol branch07.spotledger.app edge sales,stock branch-standard 48",
        "node_suspend branch07.spotledger.app",
        "node_resume branch07.spotledger.app",
        "node_revoke branch07.spotledger.app",
        "node_rotate_certificate branch07.spotledger.app",
        "node_resend branch07.spotledger.app 3:41",
        "node_notify_contact branch07.spotledger.app",
        "condition_list",
        "quarantine_list",
        "quarantine_retry q-0192f1c0",
        "quarantine_discard q-0192f1c0 superseded",
        "policy_show Sales_Invoice",
        "policy_lint",
        "role_change hub",
        "bootstrap_status",
    ];
    let samples = samples();
    assert_eq!(samples.len(), expected.len());
    for (inv, call) in samples.into_iter().zip(expected) {
        let fake = Fake::default();
        let value = dispatch(&catalogue.validate(&inv).unwrap(), &fake)
            .await
            .unwrap();
        assert!(value.is_object(), "{}", inv.op.as_str());
        assert_eq!(fake.calls(), vec![call.to_owned()], "{}", inv.op.as_str());
    }
}

#[tokio::test]
async fn the_health_result_is_the_health_view_unchanged() {
    let fake = Fake::default();
    let value = dispatch(&validated(ids::HEALTH, &[]), &fake).await.unwrap();
    let expected = spot_sync_health::build(
        &[],
        &[],
        spot_conditions::Millis::new(1_000),
        &spot_conditions::HealthThresholds::new(spot_conditions::Millis::new(60_000)),
    );
    assert_eq!(value, serde_json::to_value(&expected).unwrap());
    assert_eq!(value["status"], "calm");
}

#[tokio::test]
async fn results_serialise_to_stable_json() {
    let fake = Fake::default();
    let value = dispatch(&validated(ids::BOOTSTRAP_STATUS, &[]), &fake)
        .await
        .unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "state": "running",
            "ranges_total": 10,
            "ranges_done": 4,
            "position": {"generation": 3, "seq": 41}
        })
    );
    let value = dispatch(
        &validated(ids::NODE_SUSPEND, &[("node", text("n1"))]),
        &fake,
    )
    .await
    .unwrap();
    assert_eq!(
        value,
        serde_json::json!({"node": "n1", "state": "suspended"})
    );
}

#[tokio::test]
async fn the_enrolment_code_is_in_the_result_once_and_nowhere_else() {
    let fake = Fake::default();
    let validated = validated(
        ids::NODE_ENROL,
        &[
            ("node", text("branch07")),
            ("partitions", list(&["sales"])),
            ("subscription", text("branch-standard")),
        ],
    );
    let value = dispatch(&validated, &fake).await.unwrap();
    assert_eq!(value["code"], ENROL_CODE);
    assert_eq!(value["enrolment_id"], "enr-1");
    // The invocation, its debug form and the recorded call carry no code.
    assert!(!format!("{validated:?}").contains(ENROL_CODE));
    assert!(fake.calls().iter().all(|call| !call.contains(ENROL_CODE)));
    // The typed result never debug-prints it.
    let typed = spot_mgmt_sync::results::EnrolResult {
        enrolment_id: "enr-1".into(),
        node: "branch07".into(),
        expires_at: "2026-11-14T00:00:00Z".into(),
        code: Some(spot_mgmt_sync::OneTimeCode::new(ENROL_CODE.to_owned())),
    };
    assert!(!format!("{typed:?}").contains(ENROL_CODE));
    assert!(format!("{typed:?}").contains("<redacted>"));
}

#[tokio::test]
async fn an_unknown_operation_is_refused_and_nothing_runs() {
    let fake = Fake::default();
    for op in [
        "os.service.restart",
        "sync.node.delete",
        "sync.node.list.now",
    ] {
        let error = dispatch(&forged(op, &[]), &fake).await.unwrap_err();
        assert!(
            matches!(error, OpError::UnknownOperation { .. }),
            "{op}: {error:?}"
        );
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn an_undeclared_parameter_is_refused_even_on_a_forged_value() {
    let fake = Fake::default();
    for (op, extra) in [
        (ids::HEALTH, ("tenant", text("other-tenant"))),
        (ids::NODE_LIST, ("path", text("/tmp/x"))),
        (ids::NODE_SUSPEND, ("tenant", text("other-tenant"))),
        (ids::NODE_REVOKE, ("url", text("https://example.org"))),
        (ids::QUARANTINE_LIST, ("tenant", text("other-tenant"))),
        (ids::ROLE_CHANGE, ("tenant", text("other-tenant"))),
    ] {
        let mut params = vec![extra];
        match op {
            ids::NODE_SUSPEND => params.push(("node", text("n1"))),
            ids::NODE_REVOKE => {
                params.push(("node", text("n1")));
                params.push(("confirm", text("n1")));
            }
            ids::ROLE_CHANGE => {
                params.push(("to", text("hub")));
                params.push(("confirm", text("hub")));
            }
            _ => {}
        }
        refused(dispatch(&forged(op, &params), &fake).await);
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn confirmations_must_equal_their_target_even_on_a_forged_value() {
    let fake = Fake::default();
    for confirm in ["", "n2", "N1", "n1 ", "yes", "erase"] {
        refused(
            dispatch(
                &forged(
                    ids::NODE_REVOKE,
                    &[("node", text("n1")), ("confirm", text(confirm))],
                ),
                &fake,
            )
            .await,
        );
        refused(
            dispatch(
                &forged(
                    ids::QUARANTINE_DISCARD,
                    &[
                        ("item", text("q-1")),
                        ("reason_code", text("superseded")),
                        ("confirm", text(confirm)),
                    ],
                ),
                &fake,
            )
            .await,
        );
        refused(
            dispatch(
                &forged(
                    ids::ROLE_CHANGE,
                    &[("to", text("hub")), ("confirm", text(confirm))],
                ),
                &fake,
            )
            .await,
        );
    }
    // A missing confirm is refused too.
    refused(dispatch(&forged(ids::NODE_REVOKE, &[("node", text("n1"))]), &fake).await);
    refused(
        dispatch(
            &forged(
                ids::QUARANTINE_DISCARD,
                &[("item", text("q-1")), ("reason_code", text("superseded"))],
            ),
            &fake,
        )
        .await,
    );
    refused(dispatch(&forged(ids::ROLE_CHANGE, &[("to", text("hub"))]), &fake).await);
    assert!(fake.calls().is_empty());
    // The right confirm runs exactly once each.
    dispatch(
        &forged(
            ids::NODE_REVOKE,
            &[("node", text("n1")), ("confirm", text("n1"))],
        ),
        &fake,
    )
    .await
    .unwrap();
    assert_eq!(fake.calls(), vec!["node_revoke n1".to_owned()]);
}

#[tokio::test]
async fn enrol_refuses_a_hub_empty_partitions_and_a_bad_validity_on_a_forged_value() {
    let fake = Fake::default();
    let base = |role: &str, partitions: ParamValue, hours: ParamValue| {
        forged(
            ids::NODE_ENROL,
            &[
                ("node", text("n1")),
                ("role", text(role)),
                ("partitions", partitions),
                ("subscription", text("preset")),
                ("valid_hours", hours),
            ],
        )
    };
    refused(dispatch(&base("hub", list(&["sales"]), ParamValue::Int(24)), &fake).await);
    refused(dispatch(&base("edge", list(&[]), ParamValue::Int(24)), &fake).await);
    refused(dispatch(&base("edge", text("sales"), ParamValue::Int(24)), &fake).await);
    refused(
        dispatch(
            &base(
                "edge",
                ParamValue::List(vec![ParamValue::Int(1)]),
                ParamValue::Int(24),
            ),
            &fake,
        )
        .await,
    );
    for hours in [0, 169, -5, 70_000] {
        refused(
            dispatch(
                &base("edge", list(&["sales"]), ParamValue::Int(hours)),
                &fake,
            )
            .await,
        );
    }
    refused(dispatch(&base("edge", list(&["sales"]), text("24")), &fake).await);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn resend_from_is_optional_and_parsed() {
    let fake = Fake::default();
    dispatch(&validated(ids::NODE_RESEND, &[("node", text("n1"))]), &fake)
        .await
        .unwrap();
    dispatch(
        &validated(
            ids::NODE_RESEND,
            &[("node", text("n1")), ("from", text("0:7"))],
        ),
        &fake,
    )
    .await
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            "node_resend n1 none".to_owned(),
            "node_resend n1 0:7".to_owned()
        ]
    );
    for bad in ["x", "1", "1:y"] {
        refused(
            dispatch(
                &forged(
                    ids::NODE_RESEND,
                    &[("node", text("n1")), ("from", text(bad))],
                ),
                &fake,
            )
            .await,
        );
    }
    refused(
        dispatch(
            &forged(
                ids::NODE_RESEND,
                &[("node", text("n1")), ("from", ParamValue::Int(3))],
            ),
            &fake,
        )
        .await,
    );
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn a_missing_or_wrongly_typed_node_is_refused() {
    let fake = Fake::default();
    for op in [
        ids::NODE_SHOW,
        ids::NODE_SUSPEND,
        ids::NODE_RESUME,
        ids::NODE_ROTATE_CERTIFICATE,
        ids::NODE_NOTIFY_CONTACT,
    ] {
        refused(dispatch(&forged(op, &[]), &fake).await);
        refused(dispatch(&forged(op, &[("node", ParamValue::Int(1))]), &fake).await);
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn discard_refuses_an_empty_reason_code() {
    let fake = Fake::default();
    refused(
        dispatch(
            &forged(
                ids::QUARANTINE_DISCARD,
                &[
                    ("item", text("q-1")),
                    ("reason_code", text("")),
                    ("confirm", text("q-1")),
                ],
            ),
            &fake,
        )
        .await,
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn executor_failures_come_back_as_errors_for_every_kind() {
    for kind in [
        ExecKind::Denied,
        ExecKind::NotFound,
        ExecKind::Conflict,
        ExecKind::Unavailable,
        ExecKind::Failed,
    ] {
        let fake = Fake::failing(kind);
        let catalogue = catalogue();
        for inv in samples() {
            let error = dispatch(&catalogue.validate(&inv).unwrap(), &fake)
                .await
                .unwrap_err();
            match error {
                OpError::Exec(exec) => assert_eq!(exec.kind, kind),
                other => panic!("{}: {other:?}", inv.op.as_str()),
            }
        }
    }
}

#[tokio::test]
async fn a_failed_enrolment_returns_no_code() {
    let fake = Fake::failing(ExecKind::Denied);
    let validated = validated(
        ids::NODE_ENROL,
        &[
            ("node", text("branch07")),
            ("partitions", list(&["sales"])),
            ("subscription", text("branch-standard")),
        ],
    );
    let error = dispatch(&validated, &fake).await.unwrap_err();
    assert!(!format!("{error:?}").contains(ENROL_CODE));
    assert!(!error.to_string().contains(ENROL_CODE));
}
