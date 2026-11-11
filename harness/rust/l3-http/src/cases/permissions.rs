//! Permissions and field security: the Viewer/Team Member/Manager matrix, the summary and the
//! confidential field.

use serde_json::json;

use crate::ensure;
use crate::fixture::{Fixture, MANAGER, MEMBER, VIEWER};
use crate::report::CaseResult;

/// A Viewer reads every sample DocType and may not create, write or delete.
pub async fn the_viewer_reads_but_cannot_create_or_write(fixture: &Fixture) -> CaseResult {
    let viewer = fixture.sign_in(VIEWER).await?;
    let (project_id, _, _) = fixture.project("Viewer read").await?;
    let (task_id, task_version) = fixture.task(&project_id, "Viewer task").await?;
    viewer.document("task", &task_id).await?.expect(200)?;
    let create = viewer
        .create(
            "project",
            &json!({"code": fixture.unique("PRJ"), "title": "No",
                    "owner": fixture.admin_id()}),
        )
        .await?;
    create.expect_error("permission.denied")?;
    ensure!(
        create.status == 403,
        "the Viewer's create answers HTTP {}, not 403: {}",
        create.status,
        create.text()
    );
    let update = viewer
        .update("task", &task_id, task_version, &json!({"subject": "No"}))
        .await?;
    update.expect_error("permission.denied")?;
    let delete = viewer.delete("task", &task_id, task_version).await?;
    delete.expect_error("permission.denied")?;
    Ok(())
}

/// A Team Member creates Tasks and Time Entries, and may write only their own Time Entries.
pub async fn the_member_writes_tasks_and_only_their_own_time_entries(
    fixture: &Fixture,
) -> CaseResult {
    let member = fixture.sign_in(MEMBER).await?;
    let member_id = fixture.user(MEMBER).id.clone();
    let (project_id, _, _) = fixture.project("Member").await?;
    let task = member
        .create(
            "task",
            &json!({"subject": "Member task", "project": project_id}),
        )
        .await?;
    task.expect(201)?;
    let own = member
        .create(
            "time_entry",
            &json!({"task": task.id()?, "person": member_id,
                    "created_by": member_id, "work_date": "2026-10-20", "hours": "1.00"}),
        )
        .await?;
    own.expect(201)?;
    let own_edit = member
        .update(
            "time_entry",
            &own.id()?,
            own.version()?,
            &json!({"note": "mine"}),
        )
        .await?;
    own_edit.expect(200)?;
    let (admin_entry, admin_version) = fixture
        .time_entry(&task.id()?, "1.00", "2026-10-21")
        .await?;
    let foreign = member
        .update(
            "time_entry",
            &admin_entry,
            admin_version,
            &json!({"note": "not mine"}),
        )
        .await?;
    foreign.expect_error("permission.denied")?;
    ensure!(
        foreign.status == 403,
        "writing another user's entry answers HTTP {}, not 403: {}",
        foreign.status,
        foreign.text()
    );
    Ok(())
}

/// A Project Manager holds every right on the sample DocTypes.
pub async fn the_manager_holds_every_right(fixture: &Fixture) -> CaseResult {
    let manager = fixture.sign_in(MANAGER).await?;
    let project = manager
        .create(
            "project",
            &json!({"code": fixture.unique("PRJ"), "title": "Manager",
                    "owner": fixture.admin_id(), "status": "Active"}),
        )
        .await?;
    project.expect(201)?;
    // The sample app's field grant for `confidential` carries `read`, `update` and `report`
    // but no `create`, so a Manager sets `details` on an existing document, not on create.
    let task = manager
        .create(
            "task",
            &json!({"subject": "Manager task", "project": project.id()?}),
        )
        .await?;
    task.expect(201)?;
    let patched = manager
        .update(
            "task",
            &task.id()?,
            task.version()?,
            &json!({"status": "In Progress", "details": "manager sees this"}),
        )
        .await?;
    patched.expect(200)?;
    ensure!(
        patched.field("details") == json!("manager sees this"),
        "the Manager's update did not write the confidential field: {}",
        patched.text()
    );
    let label = manager
        .create("label", &json!({"name": fixture.unique("mgr-label")}))
        .await?;
    label.expect(201)?;
    let deleted = manager
        .delete("label", &label.id()?, label.version()?)
        .await?;
    deleted.expect(204)?;
    let entry = manager
        .create(
            "time_entry",
            &json!({"task": task.id()?, "person": fixture.user(MANAGER).id,
                    "created_by": fixture.user(MANAGER).id,
                    "work_date": "2026-10-22", "hours": "3.00"}),
        )
        .await?;
    entry.expect(201)?;
    let posted = manager
        .lifecycle("time_entry", &entry.id()?, "post", entry.version()?, None)
        .await?;
    posted.expect(200)?;
    ensure!(
        posted.state()? == "posted",
        "the manager's post did not post: {}",
        posted.text()
    );
    Ok(())
}

/// The permission summary's answer matches what the same request does.
pub async fn the_permission_summary_matches_enforcement(fixture: &Fixture) -> CaseResult {
    let viewer = fixture.sign_in(VIEWER).await?;
    let summary = viewer.permissions(&["project", "task"]).await?;
    summary.expect(200)?;
    let project = summary
        .json()
        .pointer("/data/doctypes")
        .and_then(|d| d.as_array())
        .and_then(|d| d.iter().find(|entry| entry["doctype"] == json!("project")))
        .cloned()
        .expect("a project entry in the summary");
    ensure!(
        project["rights"]["read"] == json!(true),
        "the summary denies the Viewer's read: {project}"
    );
    ensure!(
        project["rights"]["create"] == json!(false) && project["rights"]["write"] == json!(false),
        "the summary grants the Viewer create or write: {project}"
    );
    let create = viewer
        .create(
            "project",
            &json!({"code": fixture.unique("PRJ"), "title": "Summary",
                    "owner": fixture.admin_id()}),
        )
        .await?;
    ensure!(
        create.status == 403 && create.error_code().as_deref() == Some("permission.denied"),
        "the summary said no create and the request answered HTTP {} `{:?}`: {}",
        create.status,
        create.error_code(),
        create.text()
    );
    Ok(())
}

/// The confidential `task.details` is hidden from a Viewer and readable by a Manager; the
/// summary names it as hidden.
pub async fn field_masking_hides_the_confidential_details(fixture: &Fixture) -> CaseResult {
    let (project_id, _, _) = fixture.project("Confidential").await?;
    let task = fixture
        .admin()
        .create(
            "task",
            &json!({"subject": "Confidential task", "project": project_id,
                    "details": "only a manager reads this"}),
        )
        .await?;
    task.expect(201)?;
    let task_id = task.id()?;
    let viewer = fixture.sign_in(VIEWER).await?;
    let seen = viewer.document("task", &task_id).await?;
    seen.expect(200)?;
    ensure!(
        seen.data().get("details").is_none(),
        "the Viewer's document carries the confidential `details`: {}",
        seen.text()
    );
    let summary = viewer.permissions(&["task"]).await?;
    let hidden = summary
        .json()
        .pointer("/data/doctypes/0/fields/hidden")
        .and_then(|h| h.as_array())
        .cloned()
        .unwrap_or_default();
    ensure!(
        hidden.iter().any(|field| field == &json!("details")),
        "the summary does not name `details` as hidden: {}",
        summary.text()
    );
    let manager = fixture.sign_in(MANAGER).await?;
    let seen = manager.document("task", &task_id).await?;
    seen.expect(200)?;
    ensure!(
        seen.field("details") == json!("only a manager reads this"),
        "the Manager cannot read the confidential `details`: {}",
        seen.text()
    );
    Ok(())
}
