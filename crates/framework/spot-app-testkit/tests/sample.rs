//! Proof (a) and (d): the sample app through the kit.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{copy_dir, patched, sample_dir, sample_harness};
use serde_json::json;
use spot_app_api::prelude::Value;
use spot_app_testkit::{AppHarness, assert};

async fn create_project(harness: &AppHarness, owner: &str) -> String {
    let pam = harness.as_user("pam").unwrap();
    let project = pam
        .create(
            "project",
            &json!({
                "code": "P-1", "title": "First", "status": "Active", "owner": owner,
                "currency": "USD", "budget": "1250.00"
            }),
        )
        .await;
    assert::assert_ok(&project);
    project.id().unwrap()
}

#[tokio::test]
async fn a_project_a_task_and_a_time_entry_roll_up_and_back() {
    let harness = sample_harness().await;
    let pam = harness
        .user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap();
    let owner = pam.to_string();
    let project = create_project(&harness, &owner).await;
    let actor = harness.as_user("pam").unwrap();
    let task = actor
        .create(
            "task",
            &json!({"subject": "Task", "project": project, "status": "Open"}),
        )
        .await;
    assert::assert_ok(&task);
    let task_id = task.id().unwrap();
    let entry = actor
        .create(
            "time_entry",
            &json!({"task": task_id, "person": owner, "created_by": owner,
                    "work_date": "2026-01-05", "hours": "2.5"}),
        )
        .await;
    assert::assert_ok(&entry);
    let entry_id = entry.id().unwrap();

    assert::assert_ok(&actor.submit("time_entry", &entry_id).await);
    let after_post = actor.get("task", &task_id).await;
    assert::assert_ok(&after_post);
    assert_eq!(
        after_post.doc().unwrap().decimal("logged_hours").unwrap(),
        Some(rust_decimal::Decimal::from_str_exact("2.5").unwrap()),
        "posting rolls the hours up"
    );

    assert::assert_ok(&actor.cancel("time_entry", &entry_id).await);
    let after_cancel = actor.get("task", &task_id).await;
    assert_eq!(
        after_cancel.doc().unwrap().decimal("logged_hours").unwrap(),
        Some(rust_decimal::Decimal::ZERO),
        "cancelling takes them off again"
    );
    assert::assert_event_published(&harness, "doc_state").await;
    assert::assert_no_orphan_links(&harness).await;
}

#[test]
fn the_patched_sample_attaches_the_confidential_group_to_details() {
    let text = std::fs::read_to_string(sample_dir().join("doctypes/task.toml")).unwrap();
    let text = text.replace("\r\n", "\n");
    assert!(
        text.contains("[[protection_groups]]\nname = \"confidential\""),
        "the group is declared"
    );
    let details = text
        .find("name = \"details\"")
        .expect("the details field is in the sample");
    let field = text[details..].split("[[fields]]").next().unwrap();
    assert!(
        field.contains("protection_group = \"confidential\""),
        "the `details` field is in the confidential group: {field}"
    );
}

#[test]
fn a_patch_that_matches_nothing_is_refused_not_ignored() {
    assert_eq!(patched("a\nb", &[("a\nb", "c")]), "c");
    let outcome = std::panic::catch_unwind(|| patched("a\r\nb", &[("a\nb", "c")]));
    assert!(outcome.is_err(), "a pattern that does not match panics");
}

#[tokio::test]
async fn a_viewer_cannot_create_and_cannot_read_the_confidential_details() {
    let harness = sample_harness().await;
    let pam = harness
        .user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap();
    harness.user("vic").with_roles(&["Viewer"]).await.unwrap();
    let project = create_project(&harness, &pam.to_string()).await;

    let vic = harness.as_user("vic").unwrap();
    let denied = vic
        .create(
            "project",
            &json!({"code": "P-2", "title": "Nope", "status": "Active",
                    "owner": pam.to_string(), "currency": "USD"}),
        )
        .await;
    assert::assert_denied(&denied);

    let manager = harness.as_user("pam").unwrap();
    let task = manager
        .create(
            "task",
            &json!({"subject": "T", "project": project, "status": "Open"}),
        )
        .await;
    assert::assert_ok(&task);
    let id = task.id().unwrap();
    // The `confidential` group opens to a Project Manager for read and update, not create: the
    // details are written by an update.
    assert::assert_ok(
        &manager
            .update("task", &id, &json!({"details": "SECRET"}))
            .await,
    );
    let seen_by_manager = manager.get("task", &id).await;
    assert_eq!(
        seen_by_manager.doc().unwrap().get("details"),
        &Value::Text("SECRET".to_owned())
    );
    let seen_by_viewer = vic.get("task", &id).await;
    assert::assert_ok(&seen_by_viewer);
    assert_ne!(
        seen_by_viewer.doc().unwrap().get("details"),
        &Value::Text("SECRET".to_owned()),
        "the confidential details are hidden or masked for a Viewer"
    );
}

#[tokio::test]
async fn a_role_missing_write_is_caught_by_assert_denied() {
    // A copy of the sample whose Team Member may read and create tasks but not write them.
    let copy = tempfile::tempdir().unwrap();
    copy_dir(&sample_dir(), copy.path());
    std::fs::write(
        copy.path().join("permissions/grants.toml"),
        r#"
[[entries]]
role = "Project Manager"
doctype = "project"
rights = ["read", "write", "create", "delete"]

[[entries]]
role = "Project Manager"
doctype = "task"
rights = ["read", "write", "create", "delete"]

[[entries]]
role = "Team Member"
doctype = "project"
rights = ["read"]

[[entries]]
role = "Team Member"
doctype = "task"
rights = ["read", "create"]
"#,
    )
    .unwrap();
    let harness = AppHarness::builder()
        .app(copy.path())
        .controllers("sample", spot_sample_app::controllers())
        .build()
        .await
        .unwrap();
    let pam = harness
        .user("pam")
        .with_roles(&["Project Manager"])
        .await
        .unwrap();
    harness
        .user("tim")
        .with_roles(&["Team Member"])
        .await
        .unwrap();
    let project = create_project(&harness, &pam.to_string()).await;
    let tim = harness.as_user("tim").unwrap();
    let task = tim
        .create(
            "task",
            &json!({"subject": "Mine", "project": project, "status": "Open"}),
        )
        .await;
    assert::assert_ok(&task);
    let edit = tim
        .update("task", &task.id().unwrap(), &json!({"subject": "Changed"}))
        .await;
    assert::assert_denied(&edit);
}
