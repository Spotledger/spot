//! `dispatch`: one method per operation, typed arguments, refusals that fail closed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mgmt_journal::{ExecKind, OpError, dispatch, ids};
use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};
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

#[tokio::test]
async fn every_sample_reaches_exactly_its_method_and_returns_json() {
    let catalogue = catalogue();
    let expected = [
        "status",
        "recovery_report",
        "snapshot_list 5",
        "snapshot_take",
        "verify",
        "audit_last",
        "audit_run",
        "policy_get",
        "policy_set RemoteSync { on_unreachable: Halt }",
        "reship",
        "rebuild_plan at 3:41 roll-forward",
        "rebuild_run 0123456789abcdef0123456789abcdef",
        "reader_list",
        "reader_grant auditor@example.org 30 true",
        "reader_revoke auditor@example.org",
        "subject_erase",
    ];
    for (inv, call) in samples().into_iter().zip(expected) {
        let fake = Fake::default();
        let value = dispatch(&catalogue.validate(&inv).unwrap(), &fake)
            .await
            .unwrap();
        assert!(value.is_object(), "{}", inv.op.as_str());
        assert_eq!(fake.calls(), vec![call.to_owned()], "{}", inv.op.as_str());
    }
}

#[tokio::test]
async fn results_serialise_to_stable_json() {
    let fake = Fake::default();
    let value = dispatch(&validated(ids::STATUS, &[]), &fake).await.unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "state": "ok",
            "head": {"generation": 3, "seq": 41},
            "shipped": {"generation": 3, "seq": 41},
            "unshipped_segments": 0,
            "policy": {"policy": "replica-sync"}
        })
    );
    let value = dispatch(
        &validated(
            ids::SUBJECT_ERASE,
            &[("subject", text("subject-42")), ("confirm", text("erase"))],
        ),
        &fake,
    )
    .await
    .unwrap();
    // The subject is not echoed back.
    assert!(!value.to_string().contains("subject-42"));
}

#[tokio::test]
async fn an_unknown_operation_is_refused_and_nothing_runs() {
    let fake = Fake::default();
    for op in ["os.service.restart", "journal.drop", "journal.status.now"] {
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
        (ids::STATUS, ("tenant", text("other-tenant"))),
        (ids::SNAPSHOT_TAKE, ("path", text("/tmp/x"))),
        (ids::READER_REVOKE, ("tenant", text("other-tenant"))),
    ] {
        let mut params = vec![extra];
        if op == ids::READER_REVOKE {
            params.push(("grantee", text("a")));
        }
        let error = dispatch(&forged(op, &params), &fake).await.unwrap_err();
        assert!(
            matches!(error, OpError::BadArguments { .. }),
            "{op}: {error:?}"
        );
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn erase_refuses_without_the_literal_confirm_even_on_a_forged_value() {
    let fake = Fake::default();
    for confirm in ["", "yes", "Erase", "erase "] {
        let v = forged(
            ids::SUBJECT_ERASE,
            &[("subject", text("s")), ("confirm", text(confirm))],
        );
        assert!(
            matches!(dispatch(&v, &fake).await, Err(OpError::BadArguments { .. })),
            "{confirm:?}"
        );
    }
    let missing = forged(ids::SUBJECT_ERASE, &[("subject", text("s"))]);
    assert!(matches!(
        dispatch(&missing, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn rebuild_run_refuses_an_empty_hash_and_a_pointless_combination() {
    let fake = Fake::default();
    let empty = forged(
        ids::REBUILD_RUN,
        &[
            ("target", text("latest")),
            ("mode", text("full")),
            ("confirm_plan_hash", text("")),
        ],
    );
    assert!(matches!(
        dispatch(&empty, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    let pointless = forged(
        ids::REBUILD_PLAN,
        &[("target", text("latest")), ("mode", text("point-in-time"))],
    );
    assert!(matches!(
        dispatch(&pointless, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    let bad_instant = forged(
        ids::REBUILD_PLAN,
        &[
            ("target", text("before 2026-13-40T25:61:61Z")),
            ("mode", text("point-in-time")),
        ],
    );
    assert!(matches!(
        dispatch(&bad_instant, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    let bad_target = forged(
        ids::REBUILD_PLAN,
        &[("target", text("tomorrow")), ("mode", text("full"))],
    );
    assert!(matches!(
        dispatch(&bad_target, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn policy_set_refuses_on_unreachable_for_policies_that_ignore_it() {
    let fake = Fake::default();
    let error = dispatch(
        &validated(
            ids::POLICY_SET,
            &[("policy", text("local")), ("on_unreachable", text("halt"))],
        ),
        &fake,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, OpError::BadArguments { .. }), "{error:?}");
    // Left at its default it is fine, and remote-sync defaults to degrade.
    dispatch(
        &validated(ids::POLICY_SET, &[("policy", text("local"))]),
        &fake,
    )
    .await
    .unwrap();
    dispatch(
        &validated(ids::POLICY_SET, &[("policy", text("remote-sync"))]),
        &fake,
    )
    .await
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            "policy_set Local".to_owned(),
            "policy_set RemoteSync { on_unreachable: Degrade }".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_wrongly_typed_or_out_of_range_parameter_on_a_forged_value_is_refused() {
    let fake = Fake::default();
    let v = forged(
        ids::READER_GRANT,
        &[
            ("grantee", text("a")),
            ("days", text("30")),
            ("external", ParamValue::Bool(false)),
        ],
    );
    assert!(matches!(
        dispatch(&v, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    let v = forged(
        ids::READER_GRANT,
        &[
            ("grantee", text("a")),
            ("days", ParamValue::Int(9999)),
            ("external", ParamValue::Bool(false)),
        ],
    );
    assert!(matches!(
        dispatch(&v, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
    let v = forged(ids::SNAPSHOT_LIST, &[("limit", ParamValue::Int(-1))]);
    assert!(matches!(
        dispatch(&v, &fake).await,
        Err(OpError::BadArguments { .. })
    ));
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

#[test]
fn subject_arguments_never_debug_print_the_subject() {
    let args = spot_mgmt_journal::SubjectEraseArgs {
        subject: "alice@example.org".into(),
    };
    assert!(!format!("{args:?}").contains("alice"));
}
