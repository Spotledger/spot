//! The export surface (`/api/v1/export*`, WP-INT-03.T2): a filtered export holds the rows the
//! same query lists, pages by cursor without a row twice, refuses a forged cursor, and reports a
//! deleted document as a tombstone from a watermark; a user without the `export` right is
//! denied, not served an empty export.
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host,
//! so these cases cannot use the fixture they are handed. They start their own (one more
//! database and host, a few seconds) and finish it.
//!
//! The older entry below, `export: a_filtered_export_matches_the_query`, stays blocked: the
//! committed `tests/cases_index.rs` pins its owner and reason, and that test is outside this
//! task's files. See the task's open questions.

use std::collections::BTreeSet;

use serde_json::{Value as Json, json};

use super::{CaseDef, CaseFuture};
use crate::client::Reply;
use crate::fixture::{Fixture, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// Runs after the hand-written block and before `events` (the order the cases ran in before the
/// registry).
pub const ORDER: u32 = 10;

const SECTION: &str = "Bulk export";

fn run_export(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start().await else {
            return Err(failure!("the export case could not start its own fixture"));
        };
        let result = a_filtered_export_pages_and_matches_the_list(&own).await;
        let extract = extraction_reports_a_deleted_document_as_a_tombstone(&own).await;
        let denied = a_user_without_the_export_right_is_denied(&own).await;
        own.finish().await;
        result.and(extract).and(denied)
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "export: a_filtered_export_matches_the_query",
            "Blocked by a surface the host does not serve yet",
            "an export under a filter holds the rows the same query lists",
            "WP-PRD-01.T4",
            "no `/api/v1/export*` route is mounted by the host",
        ),
        CaseDef::run(
            "export: a_filtered_export_pages_matches_the_list_extracts_tombstones_and_denies_a_viewer",
            SECTION,
            "`POST /export` under a filter holds the rows the same list query returns, `/export/next` continues with no row twice, a forged cursor is 422 `bulk.cursor_invalid`, `/export/extract` reports a deleted project as a tombstone, and a Viewer's export is 403 `permission.denied`, not an empty success",
            false,
            run_export,
        ),
    ]
}

fn lines(reply: &Reply) -> Vec<Json> {
    reply
        .text()
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

async fn a_filtered_export_pages_and_matches_the_list(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let mut expected = BTreeSet::new();
    for n in 0..3 {
        let (_, _, code) = fixture.project(&format!("Export active {n}")).await?;
        expected.insert(code);
    }
    // One project that the filter leaves out.
    let planning = admin
        .create(
            "project",
            &json!({
                "code": fixture.unique("PRJ"),
                "title": "Export planning",
                "status": "Planning",
                "owner": fixture.admin_id(),
                "currency": "USD",
                "budget": "10.00",
            }),
        )
        .await?;
    planning.expect(201)?;

    let filter = json!({"field": "status", "op": "eq", "value": "Active"});
    let listed = admin
        .query("project", &json!({"filter": filter, "limit": 100}))
        .await?;
    listed.expect(200)?;
    let from_list: BTreeSet<String> = listed
        .rows()?
        .iter()
        .filter_map(|r| r["code"].as_str().map(str::to_owned))
        .collect();

    let spec = json!({
        "doctype": "project",
        "columns": ["code", "status"],
        "filter": filter,
        "order": [{"field": "code"}],
        "format": "ndjson",
    });
    let mut chunk = admin
        .send_json("POST", "/api/v1/export", &[], &spec)
        .await?;
    chunk.expect(200)?;
    let first_cursor = chunk.header("x-export-cursor").map(str::to_owned);
    let mut exported = Vec::new();
    for _ in 0..10_000 {
        exported.extend(
            lines(&chunk)
                .iter()
                .filter_map(|r| r["code"].as_str().map(str::to_owned)),
        );
        let Some(cursor) = chunk.header("x-export-cursor").map(str::to_owned) else {
            break;
        };
        chunk = admin
            .send_json(
                "POST",
                "/api/v1/export/next",
                &[],
                &json!({"cursor": cursor}),
            )
            .await?;
        chunk.expect(200)?;
    }
    let unique: BTreeSet<String> = exported.iter().cloned().collect();
    ensure!(
        unique.len() == exported.len(),
        "a row was exported twice: {exported:?}"
    );
    ensure!(
        unique == from_list && expected.is_subset(&unique),
        "the export {unique:?} differs from the list {from_list:?}"
    );

    if let Some(cursor) = first_cursor {
        let mut forged = cursor.clone();
        let last = forged.pop().unwrap_or('A');
        forged.push(if last == 'A' { 'B' } else { 'A' });
        let refused = admin
            .send_json(
                "POST",
                "/api/v1/export/next",
                &[],
                &json!({"cursor": forged}),
            )
            .await?;
        refused.expect_error("bulk.cursor_invalid")?;
        ensure!(
            refused.status == 422,
            "a forged cursor answered HTTP {}, not 422",
            refused.status
        );
    }
    Ok(())
}

async fn extraction_reports_a_deleted_document_as_a_tombstone(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (kept, _, _) = fixture.project("Extract kept").await?;
    let (doomed, version, _) = fixture.project("Extract doomed").await?;
    let full = admin
        .send_json(
            "POST",
            "/api/v1/export",
            &[],
            &json!({"doctype": "project", "columns": ["code"], "format": "ndjson"}),
        )
        .await?;
    full.expect(200)?;
    let watermark = full
        .header("x-export-watermark")
        .map(str::to_owned)
        .ok_or_else(|| failure!("the export answered no X-Export-Watermark"))?;
    admin
        .delete("project", &doomed, version)
        .await?
        .expect(204)?;
    let (added, _, _) = fixture.project("Extract added").await?;
    let page = admin
        .send_json(
            "POST",
            "/api/v1/export/extract",
            &[],
            &json!({"doctype": "project", "columns": ["code"], "watermark": watermark}),
        )
        .await?;
    page.expect(200)?;
    let data = page.data();
    let deletes: Vec<&str> = data["deletes"]
        .as_array()
        .map(|d| d.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    ensure!(
        deletes.contains(&doomed.as_str()),
        "the deleted project is not a tombstone: {data}"
    );
    let upserts: Vec<&str> = data["upserts"]
        .as_array()
        .map(|u| u.iter().filter_map(|r| r["id"].as_str()).collect())
        .unwrap_or_default();
    ensure!(
        upserts.contains(&added.as_str()) && !upserts.contains(&doomed.as_str()),
        "the upserts are wrong: {data}"
    );
    ensure!(
        !kept.is_empty() && data["new_watermark"].is_string(),
        "the page carries no new watermark: {data}"
    );
    Ok(())
}

async fn a_user_without_the_export_right_is_denied(fixture: &Fixture) -> CaseResult {
    let viewer = fixture.sign_in(VIEWER).await?;
    let denied = viewer
        .send_json(
            "POST",
            "/api/v1/export",
            &[],
            &json!({"doctype": "project", "format": "csv"}),
        )
        .await?;
    denied.expect_error("permission.denied")?;
    ensure!(
        denied.status == 403,
        "the export answered HTTP {}, not 403: {}",
        denied.status,
        denied.text()
    );
    Ok(())
}
