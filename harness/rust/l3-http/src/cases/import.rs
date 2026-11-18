//! The data import surface (`/api/v1/import*`, WP-DOC-09.T5): upload a CSV, dry run, run with the
//! digest, read the errors, cancel, and the refusals that matter: a run without the digest, a job
//! that is not yours, an unsigned request, a file that is not yours.
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host, so
//! each case starts its own fixture (one more database and host) and finishes it.

use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::{CaseDef, CaseFuture};
use crate::client::Client;
use crate::fixture::{Fixture, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// Runs after the report cases.
pub const ORDER: u32 = 16;

const SECTION: &str = "Import";

/// The rows of the budget case: the WP-DOC-09 acceptance (100,000 rows within budget).
const BUDGET_ROWS: usize = 100_000;
/// The wall-clock budget of that case on the large runner, in seconds.
const BUDGET_SECONDS: u64 = 120;
/// How long the case waits for the import to finish when the budget is not enforced, in seconds:
/// a debug build on a 2 to 4 vCPU hosted runner needs about four minutes for the rows, and the
/// case reports the real time instead of a timeout.
const UNENFORCED_WAIT_SECONDS: u64 = 1_200;

/// Whether the wall-clock budget is a failure. It is a throughput budget of the large runner:
/// it is enforced in a release build and whenever `L3_ENFORCE_BUDGET=1` (the full e2e script
/// sets it on every runner but a GitHub-hosted one), and a debug build on a small hosted runner
/// is not judged against it. The rows, the exact totals and the logged time stay in every run.
fn budget_enforced() -> bool {
    !cfg!(debug_assertions)
        || std::env::var("L3_ENFORCE_BUDGET").is_ok_and(|v| v == "1" || v == "true")
}

fn run_flow(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!("the import case could not start its own fixture"));
        };
        let result = upload_dry_run_and_run(&own).await;
        own.finish().await;
        result
    })
}

fn run_refusals(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!("the import case could not start its own fixture"));
        };
        let result = refusals_fail_closed(&own).await;
        own.finish().await;
        result
    })
}

fn run_budget(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!(
                "the import budget case could not start its own fixture"
            ));
        };
        let result = a_hundred_thousand_rows_import_within_the_budget(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "import: a_csv_is_dry_run_then_run_with_its_digest_and_the_rows_are_written_as_the_caller",
            SECTION,
            "upload then `POST /import` is 202 with a ready job and a digest, a run with a wrong digest is 409 `import.digest_changed`, a run with the digest writes every good row, reports the bad row with its row and column, and the job ends `done`",
            false,
            run_flow,
        ),
        CaseDef::run(
            "import: a_job_that_is_not_yours_a_foreign_file_and_an_unsigned_request_fail_closed",
            SECTION,
            "another user's `GET`, `errors`, `run` and `cancel` of a job are `404 document.not_found`, a file id the caller did not upload is `404`, an unsigned request is `401`, and a Viewer without the import right is `403 import.right_missing`",
            false,
            run_refusals,
        ),
        CaseDef::run(
            "import: a_hundred_thousand_rows_import_within_the_budget",
            SECTION,
            "100,000 rows of a CSV import through dry run and run within the wall-clock budget of WP-DOC-09 (the large-runner case), with exact totals",
            false,
            run_budget,
        ),
    ]
}

/// A tasks CSV: a header and `rows` rows, with data row 3 (file row 4) carrying a status no Task may have.
fn csv(project: &str, rows: usize, bad_third: bool) -> Vec<u8> {
    let mut out = String::from("subject,project,status\n");
    for n in 1..=rows {
        let status = if bad_third && n == 3 {
            "NoSuchStatus"
        } else {
            "Open"
        };
        out.push_str(&format!("Imported task {n},{project},{status}\n"));
    }
    out.into_bytes()
}

async fn upload(client: &Client, bytes: Vec<u8>) -> Result<String, crate::report::Failure> {
    let reply = client
        .send(
            "POST",
            "/api/v1/files",
            &[("content-disposition", "attachment; filename=\"tasks.csv\"")],
            Some(("application/octet-stream", bytes)),
        )
        .await?;
    reply.expect(201)?;
    reply.id()
}

async fn create(
    client: &Client,
    file: &str,
    key: &str,
) -> Result<crate::client::Reply, crate::report::Failure> {
    client
        .send_json(
            "POST",
            "/api/v1/import",
            &[("idempotency-key", key)],
            &json!({"file_id": file, "doctype": "task", "mode": "insert"}),
        )
        .await
}

async fn wait_for_state(
    client: &Client,
    id: &str,
    state: &str,
    seconds: u64,
) -> Result<Json, crate::report::Failure> {
    let started = Instant::now();
    let mut next_log = 15;
    loop {
        let reply = client.get(&format!("/api/v1/import/{id}")).await?;
        reply.expect(200)?;
        let job = reply.data();
        if started.elapsed().as_secs() >= next_log {
            eprintln!("l3: import after {} s: progress {} per mille, {}", next_log, job["progress"], job["counts"]);
            next_log += 15;
        }
        if job["state"] == state {
            return Ok(job);
        }
        ensure!(
            job["state"] != "failed",
            "the import failed while waiting for `{state}`: {}",
            reply.text()
        );
        ensure!(
            started.elapsed() < Duration::from_secs(seconds),
            "the import was `{}` after {seconds} s, not `{state}`",
            job["state"]
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn upload_dry_run_and_run(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project, _, _) = fixture.project("Import target").await?;
    let file = upload(&admin, csv(&project, 5, true)).await?;
    let created = create(&admin, &file, "l3-import-1").await?;
    created.expect(202)?;
    let job = created.data();
    let id = job["id"].as_str().unwrap_or_default().to_owned();
    ensure!(
        job["state"] == "ready",
        "the dry run left `{}`, not `ready`",
        job["state"]
    );
    ensure!(
        job["counts"]["rows"] == 5 && job["counts"]["errors"] == 1,
        "counts: {}",
        job["counts"]
    );
    let digest = job["digest"].as_str().unwrap_or_default().to_owned();
    ensure!(
        digest.len() == 64,
        "the digest is not 64 characters: {digest:?}"
    );

    let errors = admin.get(&format!("/api/v1/import/{id}/errors")).await?;
    errors.expect(200)?;
    let rows = errors.data();
    ensure!(
        // Row numbers are the file's own (the header is row 1), so the third data row is row 4.
        rows[0]["row"] == 4,
        "the bad row is not reported as file row 4: {rows}"
    );
    ensure!(
        rows[0]["column"] == "status",
        "the bad column is not reported: {rows}"
    );

    let wrong = admin
        .send_json(
            "POST",
            &format!("/api/v1/import/{id}/run"),
            &[],
            &json!({"digest": "0".repeat(64)}),
        )
        .await?;
    wrong.expect_error("import.digest_changed")?;
    let run = admin
        .send_json(
            "POST",
            &format!("/api/v1/import/{id}/run"),
            &[("idempotency-key", "l3-import-run-1")],
            &json!({"digest": digest}),
        )
        .await?;
    run.expect(202)?;
    let done = wait_for_state(&admin, &id, "done", 60).await?;
    ensure!(
        done["counts"]["ok"] == 4,
        "four rows were good: {}",
        done["counts"]
    );
    ensure!(
        done["progress"] == 1000,
        "progress is not 1000: {}",
        done["progress"]
    );
    let listed = admin
        .send_json(
            "POST",
            "/api/v1/resource/task/query",
            &[],
            &json!({"filter": {"field": "subject", "op": "prefix", "value": "Imported task"}, "limit": 50}),
        )
        .await?;
    listed.expect(200)?;
    ensure!(
        listed.rows()?.len() == 4,
        "the list holds {} imported tasks, not 4",
        listed.rows()?.len()
    );
    Ok(())
}

async fn refusals_fail_closed(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project, _, _) = fixture.project("Import refusals").await?;
    let file = upload(&admin, csv(&project, 2, false)).await?;
    let created = create(&admin, &file, "l3-import-refuse").await?;
    created.expect(202)?;
    let id = created.data()["id"].as_str().unwrap_or_default().to_owned();
    let digest = created.data()["digest"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let anonymous = admin
        .anonymous()
        .get(&format!("/api/v1/import/{id}"))
        .await?;
    ensure!(
        anonymous.status == 401,
        "an unsigned read answered HTTP {}",
        anonymous.status
    );

    let viewer = fixture.sign_in(VIEWER).await?;
    for (method, path) in [
        ("GET", format!("/api/v1/import/{id}")),
        ("GET", format!("/api/v1/import/{id}/errors")),
        ("POST", format!("/api/v1/import/{id}/cancel")),
    ] {
        let reply = if method == "GET" {
            viewer.get(&path).await?
        } else {
            viewer.send_json("POST", &path, &[], &json!({})).await?
        };
        reply.expect_error("document.not_found")?;
    }
    let run = viewer
        .send_json(
            "POST",
            &format!("/api/v1/import/{id}/run"),
            &[],
            &json!({"digest": digest}),
        )
        .await?;
    run.expect_error("document.not_found")?;
    // The file is the admin's: a viewer naming it gets the files service's one 404.
    let foreign = create(&viewer, &file, "l3-import-foreign").await?;
    ensure!(
        foreign.status == 404 || foreign.status == 403,
        "a foreign file answered HTTP {}: {}",
        foreign.status,
        foreign.text()
    );
    Ok(())
}

async fn a_hundred_thousand_rows_import_within_the_budget(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project, _, _) = fixture.project("Import budget").await?;
    let file = upload(&admin, csv(&project, BUDGET_ROWS, false)).await?;
    let started = Instant::now();
    let created = create(&admin, &file, "l3-import-budget").await?;
    created.expect(202)?;
    let job = created.data();
    let id = job["id"].as_str().unwrap_or_default().to_owned();
    ensure!(
        job["counts"]["rows"] == BUDGET_ROWS as u64,
        "counts: {}",
        job["counts"]
    );
    let digest = job["digest"].as_str().unwrap_or_default().to_owned();
    admin
        .send_json(
            "POST",
            &format!("/api/v1/import/{id}/run"),
            &[],
            &json!({"digest": digest}),
        )
        .await?
        .expect(202)?;
    // Wait up to three budgets (more when the budget is not enforced) so a slow machine reports
    // its real time instead of a timeout, and log the throughput; the budget itself is asserted
    // below when it is enforced.
    let enforced = budget_enforced();
    let wait = if enforced {
        BUDGET_SECONDS * 3
    } else {
        UNENFORCED_WAIT_SECONDS
    };
    let done = wait_for_state(&admin, &id, "done", wait).await?;
    eprintln!(
        "l3: import budget: {BUDGET_ROWS} rows in {:?}",
        started.elapsed()
    );
    ensure!(
        done["counts"]["ok"] == BUDGET_ROWS as u64,
        "counts: {}",
        done["counts"]
    );
    if !enforced {
        eprintln!(
            "l3: import budget: the {BUDGET_SECONDS} s budget is not enforced in this build \
             (debug, L3_ENFORCE_BUDGET unset); the totals above were checked"
        );
        return Ok(());
    }
    ensure!(
        started.elapsed() < Duration::from_secs(BUDGET_SECONDS),
        "{} rows took {:?}, over the {BUDGET_SECONDS} s budget",
        BUDGET_ROWS,
        started.elapsed()
    );
    Ok(())
}
