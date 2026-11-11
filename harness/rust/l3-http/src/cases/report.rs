//! The report surface (`/api/v1/report*`, WP-PLT-14.T2): the catalogue answers a list for the
//! signed-in user, and a report that does not exist (or that the caller's roles do not admit) is
//! `404 report.not_found` for `describe` and `run` alike, never an empty success; an unsigned
//! request is `401`.
//!
//! The sample app ships `hours_by_task`, so a second case runs it over real data: Time Entries
//! of two Tasks are created and posted, the report is run as a Project Manager with a
//! `from_date`, and the per-task sums are exact decimals that leave out the row dated before it.
//! The report counts every row, drafts included; the case posts the entries anyway, as a site
//! would. A Viewer is not admitted: `404 report.not_found`, never an empty success.
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host,
//! so this case cannot use the fixture it is handed. It starts its own (one more database and
//! host, a few seconds) and finishes it.

use serde_json::{Value as Json, json};

use super::{CaseDef, CaseFuture};
use crate::fixture::{Fixture, MANAGER, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// Runs after the export case and before the events cases.
pub const ORDER: u32 = 15;

const SECTION: &str = "Reports";

fn run_reports(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start().await else {
            return Err(failure!("the report case could not start its own fixture"));
        };
        let result = the_catalogue_is_served_and_fails_closed(&own).await;
        own.finish().await;
        result
    })
}

fn run_hours(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start().await else {
            return Err(failure!("the report case could not start its own fixture"));
        };
        let result = hours_by_task_sums_real_time_entries(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "reports: the_catalogue_is_served_and_an_unknown_report_is_not_found_for_describe_and_run",
        SECTION,
        "`GET /report` is 200 with a list for a signed-in user and 401 without a credential, and an unknown report is 404 `report.not_found` for `GET /report/{report}` and for `POST /report/{report}/run`, the same answer for a Viewer, never an empty success",
        false,
        run_reports,
    ),
    CaseDef::run(
        "reports: hours_by_task_sums_posted_time_entries_per_task_from_a_date_and_is_not_found_for_a_viewer",
        SECTION,
        "`POST /report/hours_by_task/run` as a Project Manager returns one row per Task with the exact decimal sum of its Time Entry hours on or after `from_date`, leaves out an entry dated before it, and is 404 `report.not_found` for a Viewer",
        false,
        run_hours,
    )]
}

async fn the_catalogue_is_served_and_fails_closed(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let listed = admin.get("/api/v1/report").await?;
    listed.expect(200)?;
    ensure!(
        listed.data().is_array(),
        "the catalogue is not an array: {}",
        listed.text()
    );

    let anonymous = admin.anonymous().get("/api/v1/report").await?;
    ensure!(
        anonymous.status == 401,
        "an unsigned catalogue request answered HTTP {}, not 401",
        anonymous.status
    );

    let viewer = fixture.sign_in(VIEWER).await?;
    for client in [&admin, &viewer] {
        let described = client.get("/api/v1/report/no_such_report").await?;
        described.expect_error("report.not_found")?;
        ensure!(
            described.status == 404,
            "describing an unknown report answered HTTP {}, not 404",
            described.status
        );
        let run = client
            .send_json(
                "POST",
                "/api/v1/report/no_such_report/run",
                &[],
                &json!({"filters": {}}),
            )
            .await?;
        run.expect_error("report.not_found")?;
        ensure!(
            run.status == 404,
            "running an unknown report answered HTTP {}, not 404",
            run.status
        );
    }
    Ok(())
}

async fn hours_by_task_sums_real_time_entries(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project, _, _) = fixture.project("Report hours").await?;
    let (task_a, _) = fixture.task(&project, "Report task A").await?;
    let (task_b, _) = fixture.task(&project, "Report task B").await?;
    for (task, hours, date) in [
        (&task_a, "1.50", "2026-10-20"),
        (&task_a, "2.00", "2026-10-21"),
        (&task_a, "4.00", "2026-09-01"),
        (&task_b, "3.25", "2026-10-22"),
    ] {
        let (id, version) = fixture.time_entry(task, hours, date).await?;
        let posted = admin
            .lifecycle("time_entry", &id, "post", version, None)
            .await?;
        posted.expect(200)?;
    }

    let manager = fixture.sign_in(MANAGER).await?;
    let body = json!({"filters": {"from_date": "2026-10-01"}});
    let run = manager
        .send_json("POST", "/api/v1/report/hours_by_task/run", &[], &body)
        .await?;
    run.expect(200)?;
    let data = run.data();
    let mut sums: Vec<(String, String)> = Vec::new();
    for row in data["rows"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let (Some(task), Some(hours)) = (row[0].as_str(), row[1].as_str()) else {
            return Err(failure!("a report row is not (task, hours): {row}"));
        };
        // The seeded sample Time Entries sum into rows of their own: only this case's Tasks count.
        if task == task_a || task == task_b {
            sums.push((task.to_owned(), hours.to_owned()));
        }
    }
    let mut expected = vec![
        (task_a.clone(), "3.50".to_owned()),
        (task_b.clone(), "3.25".to_owned()),
    ];
    expected.sort();
    sums.sort();
    ensure!(
        sums == expected,
        "the per-task sums are {sums:?}, not {expected:?} (the row before from_date must be left out)"
    );
    ensure!(
        data["truncated"] == Json::Bool(false),
        "the report says it was truncated: {}",
        run.text()
    );

    let viewer = fixture.sign_in(VIEWER).await?;
    let hidden = viewer
        .send_json("POST", "/api/v1/report/hours_by_task/run", &[], &body)
        .await?;
    hidden.expect_error("report.not_found")?;
    ensure!(
        hidden.status == 404,
        "a Viewer running hours_by_task answered HTTP {}, not 404",
        hidden.status
    );
    Ok(())
}
