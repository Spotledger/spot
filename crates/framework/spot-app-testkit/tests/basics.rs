//! The kit's own behaviour: load errors, isolation, determinism, denial everywhere.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::sample_harness;
use serde_json::json;
use spot_app_testkit::{AppHarness, assert, codes};

#[tokio::test]
async fn an_app_that_fails_its_check_reports_every_problem_at_once() {
    let empty = tempfile::tempdir().unwrap();
    let error = AppHarness::load(empty.path())
        .await
        .err()
        .expect("an empty directory is no app");
    assert_eq!(error.code, codes::APP_INVALID);
    assert!(!error.problems.is_empty(), "{error}");
    let text = error.to_string();
    assert!(text.contains(codes::APP_INVALID));
}

#[tokio::test]
async fn an_undeclared_user_is_named_not_guessed() {
    let harness = sample_harness().await;
    assert_eq!(
        harness.as_user("nobody").err().expect("unknown").code,
        codes::UNKNOWN_USER
    );
}

#[tokio::test]
async fn two_harnesses_in_one_process_share_no_state() {
    let a = sample_harness().await;
    let b = sample_harness().await;
    let owner_a = a
        .user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap()
        .to_string();
    b.user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap();
    let project = json!({"code": "P-1", "title": "Only in A", "status": "Active",
                         "owner": owner_a, "currency": "USD"});
    assert::assert_ok(&a.as_user("pam").unwrap().create("project", &project).await);
    // The same id does not exist in the other harness.
    let id = a.as_user("pam").unwrap().create("project", &json!({
        "code": "P-2", "title": "Two", "status": "Active", "owner": owner_a, "currency": "USD"
    })).await.id().unwrap();
    let missing = b.as_user("pam").unwrap().get("project", &id).await;
    assert::assert_error(&missing, "document.not_found");
}

#[tokio::test]
async fn a_user_without_rights_is_denied_on_every_operation() {
    let harness = sample_harness().await;
    let pam = harness
        .user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap();
    harness.user("none").with_roles(&["Nothing"]).await.unwrap();
    let created = harness
        .as_user("pam")
        .unwrap()
        .create(
            "project",
            &json!({"code": "P-9", "title": "T", "status": "Active",
                                   "owner": pam.to_string(), "currency": "USD"}),
        )
        .await;
    assert::assert_ok(&created);
    let id = created.id().unwrap();
    let none = harness.as_user("none").unwrap();
    assert::assert_denied(
        &none
            .create(
                "project",
                &json!({"code": "X", "title": "X",
        "status": "Active", "owner": pam.to_string(), "currency": "USD"}),
            )
            .await,
    );
    for result in [
        none.get("project", &id).await,
        none.update("project", &id, &json!({"title": "No"})).await,
        none.delete("project", &id).await,
    ] {
        assert!(!result.is_ok(), "an actor with no rights got through");
    }
}

async fn scenario() -> (String, String) {
    let harness = sample_harness().await;
    let pam = harness
        .user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap();
    let created = harness
        .as_user("pam")
        .unwrap()
        .create(
            "project",
            &json!({"code": "P-1", "title": "Same", "status": "Active",
                                   "owner": pam.to_string(), "currency": "USD"}),
        )
        .await;
    let doc = created.doc().unwrap();
    (
        created.id().unwrap(),
        doc.version().map_or_else(String::new, |v| v.0.to_string()),
    )
}

#[tokio::test]
async fn the_same_scenario_gives_the_same_ids_and_versions_twenty_times() {
    let first = scenario().await;
    for _ in 0..19 {
        assert_eq!(scenario().await, first);
    }
}
