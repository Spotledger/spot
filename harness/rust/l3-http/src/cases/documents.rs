//! Documents: CRUD on every sample DocType, child rows, table-multiselect, the dynamic link,
//! validation and the version precondition.

use serde_json::json;

use crate::ensure;
use crate::fixture::Fixture;
use crate::report::CaseResult;

/// Creates a Project with the Administrator and returns `(id, version, code)`.
async fn project(
    fixture: &Fixture,
    title: &str,
) -> Result<(String, i64, String), crate::report::Failure> {
    fixture.project(title).await
}

/// Every sample DocType round-trips through the API: a Project, a Label, a Task with child
/// rows and a draft Time Entry.
pub async fn create_every_sample_doctype(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, _, code) = project(fixture, "CRUD project").await?;
    let label = admin
        .create(
            "label",
            &json!({"name": fixture.unique("label"), "colour": "#336699"}),
        )
        .await?;
    label.expect(201)?;
    let task = admin
        .create(
            "task",
            &json!({"subject": "CRUD task", "project": project_id,
                    "checklist": [{"text": "first", "done": false}]}),
        )
        .await?;
    task.expect(201)?;
    ensure!(
        task.json()["data"]["checklist"]
            .as_array()
            .is_some_and(|r| r.len() == 1),
        "the Task did not keep its child row: {}",
        task.text()
    );
    ensure!(
        task.header("location").is_some(),
        "the create answered no Location header: {:?}",
        task.headers
    );
    let entry = admin
        .create(
            "time_entry",
            &json!({"task": task.id()?, "person": fixture.admin_id(),
                    "work_date": "2026-10-20", "hours": "1.25"}),
        )
        .await?;
    entry.expect(201)?;
    ensure!(
        entry.state()? == "draft",
        "a new Time Entry is `{}`, not `draft`: {}",
        entry.state()?,
        entry.text()
    );
    let read_back = admin.document("project", &project_id).await?;
    read_back.expect(200)?;
    ensure!(
        read_back.field("code") == json!(code),
        "the Project reads back with another code: {}",
        read_back.text()
    );
    Ok(())
}

/// A child table round-trips: a row keeps its id across an edit, a new row is appended, and a
/// row omitted from the patch is deleted.
pub async fn a_child_table_round_trips_with_its_rows(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, _, _) = project(fixture, "Child table").await?;
    let created = admin
        .create(
            "task",
            &json!({"subject": "Checklist", "project": project_id,
                    "checklist": [{"text": "one", "done": false},
                                  {"text": "two", "done": true}]}),
        )
        .await?;
    created.expect(201)?;
    let task_id = created.id()?;
    let rows = created.json()["data"]["checklist"]
        .as_array()
        .cloned()
        .expect("a checklist array");
    ensure!(rows.len() == 2, "the create kept {} rows", rows.len());
    let first_id = rows[0]["id"].as_str().expect("a row id").to_owned();
    ensure!(
        rows[0]["idx"] == json!(1) && rows[1]["idx"] == json!(2),
        "the rows are not numbered from 1: {:?}",
        rows
    );
    let patched = admin
        .update(
            "task",
            &task_id,
            created.version()?,
            &json!({"checklist": [{"id": first_id, "text": "one edited", "done": true},
                                  {"text": "three", "done": false}]}),
        )
        .await?;
    patched.expect(200)?;
    let rows = patched.json()["data"]["checklist"]
        .as_array()
        .cloned()
        .expect("a checklist array");
    ensure!(rows.len() == 2, "the patch kept {} rows", rows.len());
    ensure!(
        rows[0]["id"] == json!(first_id),
        "the edited row lost its id: {:?}",
        rows
    );
    ensure!(
        rows[0]["text"] == json!("one edited") && rows[0]["done"] == json!(true),
        "the edited row did not keep the new values: {:?}",
        rows
    );
    let deleted = admin
        .update(
            "task",
            &task_id,
            patched.version()?,
            &json!({"checklist": [{"id": first_id, "text": "one edited", "done": true}]}),
        )
        .await?;
    deleted.expect(200)?;
    let rows = deleted.json()["data"]["checklist"]
        .as_array()
        .cloned()
        .expect("a checklist array");
    ensure!(
        rows.len() == 1 && rows[0]["id"] == json!(first_id),
        "omitting a row did not delete it: {:?}",
        rows
    );
    Ok(())
}

/// `project.labels` is a table-multiselect over `label`: the picks round-trip in order.
pub async fn a_table_multiselect_round_trips(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let first = admin
        .create("label", &json!({"name": fixture.unique("pick-a")}))
        .await?;
    first.expect(201)?;
    let second = admin
        .create("label", &json!({"name": fixture.unique("pick-b")}))
        .await?;
    second.expect(201)?;
    let (first_id, second_id) = (first.id()?, second.id()?);
    let created = admin
        .create(
            "project",
            &json!({"code": fixture.unique("PRJ"), "title": "Multi pick",
                    "owner": fixture.admin_id(),
                    "labels": [{"label": first_id}, {"label": second_id}]}),
        )
        .await?;
    created.expect(201)?;
    let picks = created.json()["data"]["labels"]
        .as_array()
        .cloned()
        .expect("a labels array");
    ensure!(
        picks.len() == 2
            && picks[0]["label"] == json!(first_id)
            && picks[1]["label"] == json!(second_id),
        "the picks did not round-trip in order: {:?}",
        picks
    );
    let reordered = admin
        .update(
            "project",
            &created.id()?,
            created.version()?,
            &json!({"labels": [{"label": second_id}, {"label": first_id}]}),
        )
        .await?;
    reordered.expect(200)?;
    let picks = reordered.json()["data"]["labels"]
        .as_array()
        .cloned()
        .expect("a labels array");
    ensure!(
        picks.len() == 2
            && picks[0]["label"] == json!(second_id)
            && picks[1]["label"] == json!(first_id),
        "the reordered picks did not round-trip: {:?}",
        picks
    );
    Ok(())
}

/// A dynamic link names its target's DocType per document: one Task points at a Project and
/// another at a Task, and both read back with the type they named.
pub async fn a_dynamic_link_names_its_target_per_document(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, _) = {
        let (id, version, _) = project(fixture, "Dynamic link").await?;
        (id, version)
    };
    let (task_id, _) = fixture.task(&project_id, "Dynamic target").await?;
    let to_project = admin
        .create(
            "task",
            &json!({"subject": "Related to a project", "project": project_id,
                    "related_type": "project", "related_to": project_id}),
        )
        .await?;
    to_project.expect(201)?;
    ensure!(
        to_project.field("related_type") == json!("project")
            && to_project.field("related_to") == json!(project_id),
        "the project link did not round-trip: {}",
        to_project.text()
    );
    let to_task = admin
        .create(
            "task",
            &json!({"subject": "Related to a task", "project": project_id,
                    "related_type": "task", "related_to": task_id}),
        )
        .await?;
    to_task.expect(201)?;
    ensure!(
        to_task.field("related_type") == json!("task")
            && to_task.field("related_to") == json!(task_id),
        "the task link did not round-trip: {}",
        to_task.text()
    );
    let read = admin.document("task", &to_task.id()?).await?;
    ensure!(
        read.field("related_type") == json!("task") && read.field("related_to") == json!(task_id),
        "the stored dynamic link names another target: {}",
        read.text()
    );
    Ok(())
}

/// A field the DocType does not declare is refused, and the answer names it.
pub async fn an_unknown_field_is_refused(fixture: &Fixture) -> CaseResult {
    let reply = fixture
        .admin()
        .create(
            "project",
            &json!({"code": fixture.unique("PRJ"), "title": "Unknown",
                    "owner": fixture.admin_id(), "bogus": 1}),
        )
        .await?;
    reply.expect_error("document.unknown_field")?;
    let fields = reply
        .json()
        .pointer("/error/fields")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    ensure!(
        fields.iter().any(|f| f["field"] == json!("bogus")),
        "the refusal does not name `bogus`: {}",
        reply.text()
    );
    Ok(())
}

/// A unique field refuses a duplicate and the first document survives.
pub async fn a_unique_field_refuses_a_duplicate(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (first_id, _, code) = project(fixture, "Unique").await?;
    let duplicate = admin
        .create(
            "project",
            &json!({"code": code, "title": "Duplicate", "owner": fixture.admin_id()}),
        )
        .await?;
    duplicate.expect_error("document.duplicate")?;
    ensure!(
        duplicate.status == 409,
        "a duplicate answers HTTP {}, not 409: {}",
        duplicate.status,
        duplicate.text()
    );
    let first = admin.document("project", &first_id).await?;
    first.expect(200)?;
    Ok(())
}

/// A patch against a previous version is a conflict that names the current version.
pub async fn a_stale_version_is_a_conflict(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, version, _) = project(fixture, "Stale").await?;
    let second = admin
        .update(
            "project",
            &project_id,
            version,
            &json!({"title": "Stale two"}),
        )
        .await?;
    second.expect(200)?;
    let stale = admin
        .update(
            "project",
            &project_id,
            version,
            &json!({"title": "Stale three"}),
        )
        .await?;
    stale.expect_error("document.stale_version")?;
    ensure!(
        stale.status == 412,
        "a stale patch answers HTTP {}, not 412: {}",
        stale.status,
        stale.text()
    );
    ensure!(
        stale.error_details().get("current_version") == Some(&json!(second.version()?)),
        "the conflict does not name the current version: {}",
        stale.text()
    );
    Ok(())
}

/// A change without `If-Match` is refused as a precondition failure.
pub async fn a_patch_without_if_match_is_refused(fixture: &Fixture) -> CaseResult {
    let (project_id, _, _) = project(fixture, "No precondition").await?;
    let reply = fixture
        .admin()
        .update_unconditional("project", &project_id, &json!({"title": "Unconditional"}))
        .await?;
    reply.expect_error("api.precondition_required")?;
    ensure!(
        reply.status == 428,
        "a change without If-Match answers HTTP {}, not 428: {}",
        reply.status,
        reply.text()
    );
    Ok(())
}

/// Deleting a document another one links to is refused as `document.linked`.
pub async fn a_delete_that_a_link_blocks_lists_its_referrers(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, project_version, _) = project(fixture, "Linked").await?;
    fixture.task(&project_id, "Blocking task").await?;
    let reply = admin
        .delete("project", &project_id, project_version)
        .await?;
    reply.expect_error("document.linked")?;
    ensure!(
        reply.status == 409,
        "the blocked delete answers HTTP {}, not 409: {}",
        reply.status,
        reply.text()
    );
    // The refusal explains itself with the spot-links reason and the grouped referrers
    // (WP-DOC-06.T2); the wire code stays `document.linked`.
    let details = reply.error_details();
    ensure!(
        details.get("reason") == Some(&json!("links.referenced")),
        "the linked refusal names no `details.reason` of `links.referenced`: {}",
        reply.text()
    );
    ensure!(
        details
            .get("referrers")
            .and_then(|r| r.as_array())
            .is_some_and(|r| r.iter().any(|g| g.get("doctype") == Some(&json!("task")))),
        "the linked refusal does not list the blocking Task: {}",
        reply.text()
    );
    Ok(())
}
