#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Down-patch plans and execution through the apply engine's `StepRunner`.

mod common;

use std::sync::Arc;

use common::{FakeRunner, site};
use spot_migration_apply::{AdmissionGate, InMemoryAdmissionGate};
use spot_rescue::{DownOutcome, DownPatcher, DownRequest, Error, PatchDecl};

fn patch(id: &str, reversible: bool, down: Option<&str>) -> PatchDecl {
    let down = match down {
        Some(down) => format!("[down]\nid = \"{down}\"\n"),
        None => String::new(),
    };
    PatchDecl::parse(&format!(
        "id = \"{id}\"\nintent = \"change {id}\"\nclass = \"additive\"\n\
         reversible = {reversible}\nidempotent = true\n{down}"
    ))
    .expect("a valid patch declaration")
}

#[tokio::test]
async fn down_patches_run_in_reverse_apply_order() {
    let runner = Arc::new(FakeRunner::new());
    let gate = Arc::new(InMemoryAdmissionGate::new());
    let patcher = DownPatcher::new(site(), runner.clone(), gate.clone());
    let request = DownRequest::new(
        "plan-1",
        vec![
            patch("acme_0001", true, Some("acme_undo_0001")),
            patch("acme_0002", true, Some("acme_undo_0002")),
            patch("acme_0003", true, Some("acme_undo_0003")),
        ],
        10,
    );
    let outcome = patcher.run(request).await.expect("the run completes");
    let DownOutcome::Completed(report) = outcome else {
        panic!("expected a completed down-patch run");
    };
    assert!(report.is_complete());
    assert!(gate.status().is_open());
    assert_eq!(
        runner.calls(),
        vec![
            "acme_undo_0003".to_owned(),
            "acme_undo_0002".to_owned(),
            "acme_undo_0001".to_owned(),
        ]
    );
    assert_eq!(report.steps.len(), 3);
}

#[tokio::test]
async fn one_irreversible_patch_stops_the_plan_before_anything_runs() {
    let runner = Arc::new(FakeRunner::new());
    let gate = Arc::new(InMemoryAdmissionGate::new());
    let patcher = DownPatcher::new(site(), runner.clone(), gate.clone());
    let request = DownRequest::new(
        "plan-1",
        vec![
            patch("acme_0001", true, Some("acme_undo_0001")),
            patch("acme_0002", false, None),
            patch("acme_0003", false, None),
        ],
        10,
    );
    let error = patcher.run(request).await.expect_err("no down plan exists");
    match error {
        Error::Irreversible { patches } => {
            assert_eq!(
                patches,
                vec!["acme_0002".to_owned(), "acme_0003".to_owned()]
            );
        }
        other => panic!("expected rescue.irreversible, got {other}"),
    }
    assert!(runner.calls().is_empty());
    assert!(
        gate.status().is_open(),
        "nothing ran, so the gate was never closed"
    );
}

#[tokio::test]
async fn a_failing_down_step_keeps_the_gate_closed_and_is_reported() {
    let runner = Arc::new(FakeRunner::failing_on("acme_undo_0002"));
    let gate = Arc::new(InMemoryAdmissionGate::new());
    let patcher = DownPatcher::new(site(), runner.clone(), gate.clone());
    let request = DownRequest::new(
        "plan-1",
        vec![
            patch("acme_0001", true, Some("acme_undo_0001")),
            patch("acme_0002", true, Some("acme_undo_0002")),
        ],
        10,
    );
    let outcome = patcher
        .run(request)
        .await
        .expect("the run returns a report");
    let DownOutcome::Halted(report) = outcome else {
        panic!("expected a halted down-patch run");
    };
    assert!(!report.is_complete());
    assert!(!gate.status().is_open());
    assert_eq!(report.failures().len(), 1);
    assert_eq!(report.failures()[0].down_id, "acme_undo_0002");
    assert_eq!(report.steps.len(), 1);
    assert_eq!(runner.calls(), vec!["acme_undo_0002".to_owned()]);
}
