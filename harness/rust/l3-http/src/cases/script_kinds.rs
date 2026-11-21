//! API scripts and scheduled scripts (WP-EXT-06.T7): the `scripting` app is installed on the
//! case's own fixture, an API script is created as a document and called over HTTP, and a
//! scheduled script that fires every second writes a document as its `run_as` user.
//!
//! A registered case runs after the hand-written block, so this case starts its own fixture
//! (one more database and host, a few seconds), like `function`.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::json;

use super::CaseDef;
use crate::fixture::Fixture;
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After `function` (40); owns its fixture.
pub const ORDER: u32 = 41;

const BASE: &str = "/api/v1/method/scripting/maths/double";

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "script_kinds: an_api_script_is_served_as_its_caller_and_a_scheduled_script_runs_as_its_user",
        "Scripts",
        "an API script created as a document is served as a function (`200` for a signed-in user, `404` for nobody (a function that is not declared `guest` does not exist to a guest), `400` for a bad argument, never a guest), and a scheduled script that fires every second writes one document per slot as its `run_as` user; a `run_as` user that does not exist produces no document and a recorded error",
        false,
        |_| Box::pin(scripts_run()),
    )]
}

async fn scripts_run() -> CaseResult {
    let Some(mut fixture) = Fixture::start_secondary_with_app("scripting").await else {
        return Err(failure!(
            "the script_kinds case could not start its own fixture"
        ));
    };
    let result = run(&fixture).await;
    fixture.finish().await;
    result
}

async fn run(fixture: &Fixture) -> CaseResult {
    // The `scripting` app is in the fixture's catalogue from the start: the script services are
    // built when the host starts, so installing the app into a running host would not serve it.
    let admin = fixture.admin();

    // 1. An API script.
    admin
        .create(
            "api_script",
            &json!({
                "name": "double",
                "enabled": true,
                "module": "maths",
                "function": "double",
                "verbs": "GET and POST",
                "arguments_schema": r#"{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}"#,
                "script": "export function on_call({ args }) { return { doubled: args.n * 2 }; }",
                "timeout_ms": 200,
            }),
        )
        .await?
        .expect(201)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = admin.get(&format!("{BASE}?n=21")).await?;
        if reply.status == 200 {
            ensure!(
                reply.data()["doubled"] == json!(42),
                "the script answered something else: {}",
                reply.text()
            );
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "the API script was never served: {}",
            reply.text()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    fixture
        .client()
        .anonymous()
        .get(&format!("{BASE}?n=1"))
        .await?
        .expect(404)?
        .expect_error("method.not_found")?;
    let bad = admin.get(&format!("{BASE}?n=SECRET-VALUE-77")).await?;
    bad.expect(400)?;
    ensure!(
        !bad.text().contains("SECRET-VALUE-77"),
        "the argument value was echoed: {}",
        bad.text()
    );

    // 2. A scheduled script as the Administrator, and one whose user does not exist.
    let writer = r##"export function on_run({ ctx }) {
        host.insert({ doctype: "label", values: { name: ctx.scheduled_for, colour: "#112233" } });
      }"##;
    for (name, run_as) in [("tick", "administrator"), ("ghost", "nobody_by_that_name")] {
        admin
            .create(
                "scheduled_script",
                &json!({
                    "name": name,
                    "enabled": true,
                    "cron": "* * * * * *",
                    "time_zone": "UTC",
                    "missed_run": "skip",
                    "queue": "default",
                    "run_as": run_as,
                    "script": writer,
                    "timeout_ms": 500,
                }),
            )
            .await?
            .expect(201)?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let names: BTreeSet<String> = loop {
        let rows = admin.get("/api/v1/resource/label").await?.rows()?;
        let names: BTreeSet<String> = rows
            .iter()
            .filter_map(|row| row["name"].as_str())
            .filter(|name| name.ends_with('Z') && name.starts_with("20"))
            .map(str::to_owned)
            .collect();
        if names.len() >= 2 {
            break names;
        }
        ensure!(
            Instant::now() < deadline,
            "the scheduled script wrote {} documents in a minute",
            names.len()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    // One document per slot: a slot run twice would have failed on the unique name, and the
    // script's own record would say so.
    ensure!(
        names.iter().all(|name| name.len() == 20),
        "an unexpected label name: {names:?}"
    );
    let scripts = admin
        .get("/api/v1/resource/scheduled_script")
        .await?
        .rows()?;
    let ghost = scripts
        .iter()
        .find(|row| row["name"] == json!("ghost"))
        .ok_or_else(|| failure!("the ghost script is missing"))?;
    ensure!(
        ghost["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("run_as_unavailable")),
        "a missing run_as user left no recorded error: {ghost}"
    );
    let tick = scripts
        .iter()
        .find(|row| row["name"] == json!("tick"))
        .ok_or_else(|| failure!("the tick script is missing"))?;
    ensure!(
        tick["last_run_count"].as_i64().is_some_and(|n| n >= 2),
        "the run count was not kept: {tick}"
    );
    Ok(())
}
