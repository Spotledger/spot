//! End-to-end field-level security over HTTP (WP-SEC-04.T17): one protected value, every channel.
//!
//! The shared fixture has two grants over one protected field: `task.details` is hidden from the
//! Viewer (the denied user) and readable by the Manager (the reader). It has no masked grant and
//! no reveal grant, and no protected field a SCIM resource or a link title could carry. The two
//! cases the fixture can serve run for real. The others are registered as **blocked**, with the
//! task that owns the channel and a reason that says what is missing (a fixture grant, or a case
//! body still to be written against a running stack), so the distance stays visible in every run.
//!
//! Every case writes its own records with a canary in the protected field and asserts that the
//! canary appears in no answer a denied user receives.

use serde_json::json;

use super::{CaseDef, CaseFuture};
use crate::client::Client;
use crate::ensure;
use crate::fixture::{Fixture, MANAGER, VIEWER};
use crate::report::{CaseResult, Failure};

/// After the shared groups, before the groups that own a fixture of their own.
pub const ORDER: u32 = 55;

const SECTION: &str = "Field-level security, every channel";

const NEEDS_GRANTS: &str = "the shared fixture has only a denied (Viewer) and a reading (Manager) user over one hidden field; a masked and a revealing grant need a fixture DocType with a masked field, which `fixture.rs` does not build";
const NEEDS_SURFACE: &str = "the case body is not written yet: the channel's HTTP shape could not be observed without a running stack; it is the follow-up of WP-SEC-04.T17";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "field_security.document_read",
            SECTION,
            "a denied user's document carries no key for the protected field and not its value; a reading user's document carries it",
            false,
            run_document_read,
        ),
        CaseDef::run(
            "field_security.list_and_columns",
            SECTION,
            "a denied user filtering, sorting or projecting on the protected field gets exactly the answer a field that does not exist gets, and no answer holds the value; a reading user's filter finds the document",
            false,
            run_list_and_columns,
        ),
        CaseDef::blocked(
            "field_security.link_title_and_lookup",
            SECTION,
            "a link title and a lookup never show the protected value to a denied user",
            "WP-SEC-04.T14",
            NEEDS_GRANTS,
        ),
        CaseDef::blocked(
            "field_security.search",
            SECTION,
            "the canary is not in the search index for a denied user, a revealer finds the document, and removing the grant removes the hit",
            "WP-SEC-04.T12",
            NEEDS_SURFACE,
        ),
        CaseDef::blocked(
            "field_security.export_and_bulk",
            SECTION,
            "document export, bulk export and the incremental extraction omit the protected field for a denied user",
            "WP-INT-03.T2",
            NEEDS_SURFACE,
        ),
        CaseDef::blocked(
            "field_security.event_stream_and_notification",
            SECTION,
            "the data subscription and the realtime payload `changed` never carry the protected value for a denied user",
            "WP-SEC-04.T15",
            NEEDS_SURFACE,
        ),
        CaseDef::blocked(
            "field_security.version_history",
            SECTION,
            "the version history shows a denied user no protected value",
            "WP-SEC-04.T16",
            NEEDS_SURFACE,
        ),
        CaseDef::blocked(
            "field_security.journal_and_audit_read_model",
            SECTION,
            "the audit read model shows a non-system reader names only, never values",
            "WP-SEC-04.T11",
            NEEDS_SURFACE,
        ),
        CaseDef::blocked(
            "field_security.print_and_report",
            SECTION,
            "a print and a report omit the protected field for a user without the report right",
            "WP-SEC-04.T13",
            NEEDS_GRANTS,
        ),
        CaseDef::blocked(
            "field_security.scim",
            SECTION,
            "the SCIM resource of a user whose attribute is protected omits it for a denied caller",
            "WP-SEC-04.T14",
            NEEDS_SURFACE,
        ),
        CaseDef::blocked(
            "field_security.every_channel_listed",
            SECTION,
            "the case fails when a `spot-field-guard` `Channel` variant has no case in this file",
            "WP-SEC-04.T17",
            "`l3-http` has no dev-dependency on `spot-field-guard` and `Cargo.toml` is not an owned path of this task; the lead widens the paths and the case reads the enum then",
        ),
        CaseDef::blocked(
            "field_security.trace",
            SECTION,
            "every read that returned a value or a mask appears in the field access trace, and the trace never holds the value",
            "WP-SEC-04.T12",
            NEEDS_SURFACE,
        ),
    ]
}

// The registered cases run after the shared host was shut down (the shutdown case), so each of
// these starts a host of its own.
fn run_document_read(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut fixture) = Fixture::start_secondary().await else {
            return Err(crate::failure!(
                "the field security case could not start its own fixture"
            ));
        };
        let result = document_read(&fixture).await;
        fixture.finish().await;
        result
    })
}

fn run_list_and_columns(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut fixture) = Fixture::start_secondary().await else {
            return Err(crate::failure!(
                "the field security case could not start its own fixture"
            ));
        };
        let result = list_and_columns(&fixture).await;
        fixture.finish().await;
        result
    })
}

/// A canary no legitimate text contains.
fn canary(fixture: &Fixture) -> String {
    fixture.unique("CANARY-FIELD-SECURITY")
}

/// A task whose protected `details` holds `value`; returns its id and its subject.
async fn task_with_details(fixture: &Fixture, value: &str) -> Result<(String, String), Failure> {
    let (project_id, _, _) = fixture.project("Field security").await?;
    let subject = fixture.unique("fs-task");
    let created = fixture
        .admin()
        .create(
            "task",
            &json!({"subject": subject, "project": project_id, "details": value}),
        )
        .await?;
    created.expect(201)?;
    Ok((created.id()?, subject))
}

async fn document_read(fixture: &Fixture) -> CaseResult {
    let value = canary(fixture);
    let (id, _) = task_with_details(fixture, &value).await?;

    let denied = fixture.sign_in(VIEWER).await?;
    let seen = denied.document("task", &id).await?;
    seen.expect(200)?;
    ensure!(
        seen.data().get("details").is_none(),
        "the denied user's document has a `details` key: {}",
        seen.redacted_text()
    );
    ensure!(
        !seen.text().contains(&value),
        "the denied user's document carries the canary"
    );

    let reader = fixture.sign_in(MANAGER).await?;
    let seen = reader.document("task", &id).await?;
    seen.expect(200)?;
    ensure!(
        seen.field("details") == json!(value),
        "the reading user's document lacks the value: {}",
        seen.redacted_text()
    );
    Ok(())
}

/// The answer a request got, for comparison: the status and the error code, never the body (a
/// body may legitimately differ in the request id).
async fn answer(
    client: &Client,
    body: &serde_json::Value,
) -> Result<(u16, Option<String>), Failure> {
    let reply = client.query("task", body).await?;
    Ok((reply.status, reply.error_code()))
}

async fn list_and_columns(fixture: &Fixture) -> CaseResult {
    let value = canary(fixture);
    let (id, subject) = task_with_details(fixture, &value).await?;
    let scope = json!({"field": "subject", "op": "prefix", "value": subject});
    let denied = fixture.sign_in(VIEWER).await?;

    // The three ways to use a field: filter on it, sort by it, project it.
    type UseFn = fn(&str, &serde_json::Value, &str) -> serde_json::Value;
    let uses: [(&str, UseFn); 3] = [
        (
            "filter",
            |field, scope, value| json!({"filter": {"and": [scope, {"field": field, "op": "prefix", "value": value}]}, "limit": 20}),
        ),
        (
            "sort",
            |field, scope, _| json!({"filter": scope, "order": [{"field": field, "dir": "asc", "nulls": "last"}], "limit": 20}),
        ),
        (
            "project",
            |field, scope, _| json!({"filter": scope, "fields": ["subject", field], "limit": 20}),
        ),
    ];
    for (what, build) in uses {
        let hidden = build("details", &scope, &value);
        let absent = build("no_such_field_zz", &scope, &value);
        let hidden_answer = answer(&denied, &hidden).await?;
        let absent_answer = answer(&denied, &absent).await?;
        ensure!(
            hidden_answer == absent_answer,
            "a denied user's {what} on the protected field answers {hidden_answer:?}, a field that does not exist answers {absent_answer:?}"
        );
        let reply = denied.query("task", &hidden).await?;
        ensure!(
            !reply.text().contains(&value),
            "a denied user's {what} on the protected field returned the canary"
        );
        for row in reply.rows().unwrap_or_default() {
            ensure!(
                row.get("details").is_none(),
                "a denied user's {what} returned a row with a `details` key"
            );
        }
    }

    // The reading user's filter finds the document: the protection is not a blanket refusal.
    let reader = fixture.sign_in(MANAGER).await?;
    let found = reader
        .query(
            "task",
            &json!({"filter": {"and": [scope, {"field": "details", "op": "prefix", "value": value}]}, "limit": 20}),
        )
        .await?;
    found.expect(200)?;
    ensure!(
        found.rows()?.iter().any(|row| row["id"] == json!(id)),
        "the reading user's filter on the protected field does not find the document: {}",
        found.redacted_text()
    );
    Ok(())
}
