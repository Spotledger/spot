//! Lists, filters, sorting, cursor paging, projection and the permission-filtered list.

use serde_json::{Value as Json, json};

use crate::ensure;
use crate::fixture::{Fixture, VIEWER};
use crate::report::CaseResult;

/// The code prefix every project this module creates shares, so a filter sees only them.
fn prefix(fixture: &Fixture, tag: &str) -> String {
    format!("PRJ-{}-{tag}", &fixture.run()[..6])
}

async fn create_project(
    fixture: &Fixture,
    tag: &str,
    suffix: &str,
    title: &str,
    status: &str,
    budget: &str,
) -> Result<String, crate::report::Failure> {
    let reply = fixture
        .admin()
        .create(
            "project",
            &json!({"code": format!("{}-{suffix}", prefix(fixture, tag)), "title": title,
                    "status": status, "owner": fixture.admin_id(), "budget": budget}),
        )
        .await?;
    reply.expect(201)?;
    reply.id()
}

/// Equality and range filters select exactly the documents that match.
pub async fn a_query_filters_by_equality_and_range(fixture: &Fixture) -> CaseResult {
    let prefix = prefix(fixture, "flt");
    create_project(fixture, "flt", "eq-a", "A", "Active", "10.00").await?;
    create_project(fixture, "flt", "eq-b", "B", "Active", "200.00").await?;
    create_project(fixture, "flt", "eq-c", "C", "Planning", "300.00").await?;
    let reply = fixture
        .admin()
        .query(
            "project",
            &json!({"filter": {"and": [
                {"field": "code", "op": "prefix", "value": prefix},
                {"field": "status", "op": "eq", "value": "Active"},
                {"field": "budget", "op": "ge", "value": "100.00"}
            ]}, "limit": 50}),
        )
        .await?;
    reply.expect(200)?;
    let rows = reply.rows()?;
    ensure!(
        rows.len() == 1 && rows[0]["code"] == json!(format!("{prefix}-eq-b")),
        "the filter selected {:?}, not just the Active project over 100",
        rows.iter().map(|r| r["code"].clone()).collect::<Vec<_>>()
    );
    Ok(())
}

/// Ordering by two keys is stable: status ascending, then title descending.
pub async fn a_query_sorts_by_two_keys_and_the_id(fixture: &Fixture) -> CaseResult {
    let prefix = prefix(fixture, "srt");
    create_project(fixture, "srt", "sort-a", "Alpha", "Active", "1.00").await?;
    create_project(fixture, "srt", "sort-b", "Bravo", "Planning", "1.00").await?;
    create_project(fixture, "srt", "sort-c", "Charlie", "Active", "1.00").await?;
    let reply = fixture
        .admin()
        .query(
            "project",
            &json!({"filter": {"field": "code", "op": "prefix", "value": prefix},
                    "order": [{"field": "status", "dir": "asc", "nulls": "last"},
                              {"field": "title", "dir": "desc", "nulls": "last"}],
                    "limit": 50}),
        )
        .await?;
    reply.expect(200)?;
    let titles: Vec<Json> = reply.rows()?.iter().map(|r| r["title"].clone()).collect();
    ensure!(
        titles == vec![json!("Charlie"), json!("Alpha"), json!("Bravo")],
        "the two-key order is {titles:?}, not Charlie, Alpha, Bravo"
    );
    Ok(())
}

/// A cursor walks every row exactly once and ends with `next_cursor: null`.
pub async fn a_query_pages_with_a_cursor(fixture: &Fixture) -> CaseResult {
    let name_prefix = format!("page-{}", &fixture.run()[..6]);
    for suffix in ["a", "b", "c", "d", "e"] {
        let reply = fixture
            .admin()
            .create("label", &json!({"name": format!("{name_prefix}-{suffix}")}))
            .await?;
        reply.expect(201)?;
    }
    let filter = json!({"field": "name", "op": "prefix", "value": name_prefix});
    let mut cursor: Option<String> = None;
    let mut ids = Vec::new();
    let mut pages = 0;
    loop {
        let mut body = json!({"filter": filter, "order": [{"field": "name", "dir": "asc"}],
                              "limit": 2});
        if let Some(cursor) = &cursor {
            body["cursor"] = json!(cursor);
        }
        let reply = fixture.admin().query("label", &body).await?;
        reply.expect(200)?;
        pages += 1;
        ensure!(pages <= 10, "the cursor never reached the last page");
        for row in reply.rows()? {
            ids.push(row["id"].as_str().unwrap_or_default().to_owned());
        }
        match reply.next_cursor() {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    ensure!(
        ids.len() == 5,
        "the cursor walked {} rows, not 5",
        ids.len()
    );
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    ensure!(
        unique.len() == ids.len(),
        "the cursor returned a row twice: {ids:?}"
    );
    Ok(())
}

/// `with_total` reports the number of matching documents, not the page size.
pub async fn a_query_reports_its_total_when_asked(fixture: &Fixture) -> CaseResult {
    let name_prefix = format!("total-{}", &fixture.run()[..6]);
    for suffix in ["a", "b", "c"] {
        let reply = fixture
            .admin()
            .create("label", &json!({"name": format!("{name_prefix}-{suffix}")}))
            .await?;
        reply.expect(201)?;
    }
    let reply = fixture
        .admin()
        .query(
            "label",
            &json!({"filter": {"field": "name", "op": "prefix", "value": name_prefix},
                    "limit": 2, "with_total": true}),
        )
        .await?;
    reply.expect(200)?;
    ensure!(
        reply.rows()?.len() == 2,
        "the page carries {} rows, not 2",
        reply.rows()?.len()
    );
    ensure!(
        reply.total() == Some(3),
        "the total is {:?}, not 3: {}",
        reply.total(),
        reply.text()
    );
    Ok(())
}

/// `fields` projects only the requested fields, plus the id and version every row needs.
pub async fn a_query_projects_only_the_requested_fields(fixture: &Fixture) -> CaseResult {
    let prefix = prefix(fixture, "prj");
    create_project(fixture, "prj", "proj", "Projected", "Active", "9.99").await?;
    let reply = fixture
        .admin()
        .query(
            "project",
            &json!({"filter": {"field": "code", "op": "prefix", "value": prefix},
                    "fields": ["code", "title"], "limit": 5}),
        )
        .await?;
    reply.expect(200)?;
    let rows = reply.rows()?;
    ensure!(
        rows.len() == 1,
        "the projection selected {} rows",
        rows.len()
    );
    let row = rows[0].as_object().expect("a row object");
    let mut keys: Vec<&str> = row.keys().map(String::as_str).collect();
    keys.sort_unstable();
    ensure!(
        keys == ["code", "id", "title", "version"],
        "the projected row carries {keys:?}, not code/id/title/version"
    );
    Ok(())
}

/// Every row a Viewer's list returns is a row the Viewer may read, and a document the Viewer
/// may not read is absent from the list.
pub async fn the_permission_filtered_list_equals_a_per_document_check(
    fixture: &Fixture,
) -> CaseResult {
    // The case's own row: the non-empty assertion must not depend on a case that ran earlier.
    let (project_id, _, _) = fixture.project("List permission").await?;
    fixture.task(&project_id, "List permission task").await?;
    let viewer = fixture.sign_in(VIEWER).await?;
    let page = viewer.list("task", "limit=100").await?;
    page.expect(200)?;
    let rows = page.rows()?;
    ensure!(
        rows.iter()
            .any(|row| row["subject"] == json!("List permission task")),
        "the Viewer's task list does not carry the task this case created: {}",
        page.text()
    );
    for row in &rows {
        let id = row["id"].as_str().unwrap_or_default();
        let read = viewer.document("task", id).await?;
        ensure!(
            read.status == 200,
            "the list returned task {id} which a per-document read refuses with HTTP {}: {}",
            read.status,
            read.text()
        );
    }
    let users = viewer.list("user", "limit=100").await?;
    users.expect(200)?;
    ensure!(
        users.rows()?.is_empty(),
        "the Viewer's user list is not empty although the Viewer may not read users: {}",
        users.text()
    );
    let denied = viewer.document("user", &fixture.user(VIEWER).id).await?;
    ensure!(
        denied.status == 403,
        "the per-document read of a user answers HTTP {}, not 403: {}",
        denied.status,
        denied.text()
    );
    ensure!(
        denied.error_code().as_deref() == Some("permission.denied"),
        "the per-document read refuses with `{:?}`: {}",
        denied.error_code(),
        denied.text()
    );
    Ok(())
}
