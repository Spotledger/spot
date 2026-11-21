//! A process runs end to end over HTTP on a real `spot serve` (WP-BPM-02.T7): publish a version, start
//! a request, approve it in the inbox, the Document node acts, insight shows it, the server restarts in
//! the middle and the run completes once.
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host, so each
//! case here owns a fixture (and, where it restarts the server, a host of its own: `support::Host`).
//! Every step asserts the HTTP status, the problem code on refusals and the headers; the only direct
//! database read is `support::count_rows`, read-only, for the "exactly once" counts.
//!
//! This task writes cases and nothing else. A defect a case finds goes to the report's open questions
//! with the failing assertion; the owning task is reopened by the lead. A surface the model or the host
//! does not have yet is a **blocked** case naming its owner, never a deleted one.

#[path = "process/support.rs"]
pub mod support;

use serde_json::json;

use self::support::{
    Actors, Host, actors, admin_again, decide, definition, inbox, publish, run, sign_in_again,
    start, wait_for_state, wait_for_task, wait_ready,
};
use super::CaseDef;
use crate::fixture::{Fixture, MANAGER, MEMBER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After `function` (40), `ai_isolation` (60) and `mcp` (70); each case owns its fixture.
pub const ORDER: u32 = 80;

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "process: a_request_is_published_started_approved_and_the_document_node_acts",
            "Processes",
            "publish through `process_version_publish`; the requester starts a run (a repeat of the `Idempotency-Key` is the same run); the approver's inbox shows the task and the requester's does not; claim and complete with `If-Match`; the Document node creates its document once; `run_get` shows `completed`; the events name the requester and the approver; the cycle-time report answers integers",
            false,
            |_| Box::pin(own_end_to_end()),
        ),
        CaseDef::run(
            "process: a_server_restart_in_the_middle_completes_the_run_once",
            "Processes",
            "a run waits at the Decision node; the server is stopped and started again; the task is still there and completing it finishes the run exactly once with exactly one document",
            false,
            |_| Box::pin(own_restart()),
        ),
        CaseDef::run(
            "process: a_cancel_closes_the_task",
            "Processes",
            "the requester cancels a waiting run: `run_cancel` answers the run `cancelled`, the task leaves the approver's inbox and a repeat is `409 process.run_not_active`; a caller who may not see the run gets `404 process.run_not_found`",
            false,
            |_| Box::pin(own_cancel()),
        ),
        CaseDef::run(
            "process: a_new_version_leaves_a_running_run_on_its_own_version",
            "Processes",
            "publish v2 while a run is on v1: the run finishes on v1, new runs start on v2, `process_version_list` shows both and the migration plan names what can move",
            false,
            |_| Box::pin(own_versions()),
        ),
        CaseDef::run(
            "process: a_webhook_that_does_not_verify_is_one_identical_401",
            "Processes",
            "an unsigned delivery, a badly signed one and one for an unknown trigger are the same `401 process.webhook_rejected` with the same body, and none starts a run",
            false,
            |_| Box::pin(own_webhook()),
        ),
        CaseDef::blocked(
            "process: an_sla_reminder_arrives_and_never_auto_approves",
            "Processes",
            "a node with a short SLA sends a reminder; with the auto-approve flag unset it never auto-approves",
            "WP-BPM-05.T8",
            "the model's Decision node has no SLA setting (`spot-bpm-model::DecisionConfig` carries title, assignee and outcomes only), so a definition cannot ask for one over HTTP",
        ),
        CaseDef::blocked(
            "process: a_signed_webhook_and_a_document_event_each_start_one_run",
            "Processes",
            "a signed webhook starts one run and a redelivery none; a document-event trigger starts one run per saved document even when the event is redelivered",
            "WP-BPM-03.T4",
            "a test needs the trigger's webhook secret and a trigger node with a path or event: `TriggerConfig` has them, but no operation hands the secret back, and the host's signing secret is not reachable over HTTP",
        ),
    ]
}

macro_rules! own_fixture {
    ($name:ident, $case:ident) => {
        async fn $name() -> CaseResult {
            let Some(mut fixture) = Fixture::start_secondary().await else {
                return Err(failure!("the process case could not start its own fixture"));
            };
            let result = $case(&fixture).await;
            fixture.finish().await;
            result
        }
    };
}

own_fixture!(own_end_to_end, end_to_end);
own_fixture!(own_restart, restart);
own_fixture!(own_cancel, cancel);
own_fixture!(own_versions, versions);
own_fixture!(own_webhook, webhook);

/// Publishes `process` as version 1 and returns the actors.
async fn published(fixture: &Fixture, process: &str) -> Result<Actors, crate::report::Failure> {
    let people = actors(fixture).await?;
    let admin = fixture.admin();
    let reply = publish(
        &admin,
        process,
        &definition(process, "Approve the request"),
        &fixture.unique("pub"),
    )
    .await?;
    reply.expect(201)?;
    ensure!(
        reply.field("version") == json!(1),
        "the first publish is not version 1: {}",
        reply.text()
    );
    ensure!(
        reply.field("state") == json!("published"),
        "the first publish is not `published`: {}",
        reply.text()
    );
    Ok(people)
}

async fn label_total(fixture: &Fixture) -> Result<Option<u64>, crate::report::Failure> {
    let reply = fixture
        .admin()
        .query("label", &json!({"limit": 1, "with_total": true}))
        .await?;
    reply.expect(200)?;
    Ok(reply.total())
}

async fn end_to_end(fixture: &Fixture) -> CaseResult {
    let process = "l3_request";
    let people = published(fixture, process).await?;
    let labels_before = label_total(fixture).await?;

    // The requester sees the process and starts it; a repeat of the key is the same run.
    let listed = people.requester.get("/api/v1/process?limit=100").await?;
    listed.expect(200)?;
    ensure!(
        listed.rows()?.iter().any(|p| p["name"] == json!(process)),
        "the requester's process list lacks `{process}`: {}",
        listed.text()
    );
    let key = fixture.unique("start");
    let started = start(&people.requester, process, &key, &json!({})).await?;
    started.expect(201)?;
    let run_id = started.field("id").as_str().unwrap_or_default().to_owned();
    ensure!(
        !run_id.is_empty(),
        "the started run has no id: {}",
        started.text()
    );
    let again = start(&people.requester, process, &key, &json!({})).await?;
    again.expect(200)?;
    ensure!(
        again.header("idempotency-replayed") == Some("true") && again.field("id") == json!(run_id),
        "a repeat of the Idempotency-Key is not the same run: {}",
        again.text()
    );

    // The approver's inbox shows the task; the requester's and the unrelated user's do not.
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();
    ensure!(!task_id.is_empty(), "the task has no id: {task}");
    ensure!(
        inbox(&people.requester, process).await?.is_empty(),
        "the requester sees the approver's task in their inbox"
    );
    ensure!(
        inbox(&people.unrelated, process).await?.is_empty(),
        "an unrelated user sees the task in their inbox"
    );
    let hidden = people
        .unrelated
        .get(&format!("/api/v1/task/{task_id}"))
        .await?;
    hidden.expect_error("task.not_found")?;

    // Claim and complete with `If-Match`; the Document node then acts exactly once.
    let decided = decide(
        &people.approver,
        &task_id,
        "approve",
        &fixture.unique("decide"),
    )
    .await?;
    decided.expect(200)?;
    ensure!(
        decided.field("state") == json!("completed"),
        "the decided task is not `completed`: {}",
        decided.text()
    );
    let finished = wait_for_state(&people.requester, &run_id, "completed").await?;
    ensure!(
        finished["documents"]
            .as_array()
            .is_some_and(|docs| docs.iter().any(|d| d["doctype"] == json!("label"))),
        "the completed run names no document of the Document node: {finished}"
    );
    let labels_after = label_total(fixture).await?;
    ensure!(
        labels_after.zip(labels_before).map(|(a, b)| a - b) == Some(1),
        "the Document node created {:?} documents, not one ({labels_before:?} -> {labels_after:?})",
        labels_after
            .zip(labels_before)
            .map(|(a, b)| a.saturating_sub(b))
    );

    // The audit trail names the requester and the approver.
    let events = people
        .requester
        .get(&format!("/api/v1/process/runs/{run_id}/events"))
        .await?;
    events.expect(200)?;
    let trail = events.text();
    ensure!(
        trail.contains(&people.requester_id) && trail.contains(&people.approver_id),
        "the run's events do not name both actors: {trail}"
    );
    ensure!(
        !trail.contains("approve\":") || !trail.contains("\"input\""),
        "the run's events carry a payload: {trail}"
    );

    // Insight: the cycle-time report answers whole numbers and omits a process the viewer may not read.
    let report = fixture
        .admin()
        .send_json(
            "POST",
            "/api/v1/report/bpm_cycle_time_by_step/run",
            &[],
            &json!({}),
        )
        .await?;
    report.expect(200)?;
    ensure!(
        !report.text().contains(".0,") && !report.text().contains(".5"),
        "the cycle-time report carries a fractional number: {}",
        report.text()
    );
    let as_unrelated = people
        .unrelated
        .send_json(
            "POST",
            "/api/v1/report/bpm_cycle_time_by_step/run",
            &[],
            &json!({}),
        )
        .await?;
    ensure!(
        !as_unrelated.text().contains(process) || as_unrelated.status == 404,
        "a viewer who may not read the process sees it in the insight report: {}",
        as_unrelated.text()
    );
    Ok(())
}

async fn restart(fixture: &Fixture) -> CaseResult {
    let process = "l3_restart";
    let people = published(fixture, process).await?;
    let started = start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({}),
    )
    .await?;
    started.expect(201)?;
    let run_id = started.field("id").as_str().unwrap_or_default().to_owned();
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();
    let labels_before = label_total(fixture).await?;

    // Stop the server in the middle of the run and start another on the same root.
    let host = Host::restart(fixture).await?;
    let outcome = after_restart(fixture, &host, process, &run_id, &task_id, labels_before).await;
    host.stop().await;
    outcome
}

async fn after_restart(
    fixture: &Fixture,
    host: &Host,
    process: &str,
    run_id: &str,
    task_id: &str,
    labels_before: Option<u64>,
) -> CaseResult {
    let site = fixture.site();
    wait_ready(&host.client(site)).await?;
    let requester = sign_in_again(host, site, fixture, MEMBER).await?;
    let approver = sign_in_again(host, site, fixture, MANAGER).await?;
    let waiting = run(&requester, run_id).await?;
    waiting.expect(200)?;
    ensure!(
        waiting.field("state") == json!("waiting") || waiting.field("state") == json!("running"),
        "after the restart the run is `{}`, not waiting: {}",
        waiting.field("state"),
        waiting.text()
    );
    let still = wait_for_task(&approver, process).await?;
    ensure!(
        still["id"].as_str() == Some(task_id),
        "the task after the restart is a different one: {still}"
    );
    let decided = decide(&approver, task_id, "approve", &fixture.unique("decide")).await?;
    decided.expect(200)?;
    wait_for_state(&requester, run_id, "completed").await?;
    // Exactly once: one more document, and completing again is the same success, not a second run.
    let admin = admin_again(host, site).await?;
    let after = admin
        .query("label", &json!({"limit": 1, "with_total": true}))
        .await?;
    after.expect(200)?;
    ensure!(
        after.total().zip(labels_before).map(|(a, b)| a - b) == Some(1),
        "the restart run created {:?} documents, not one",
        after
            .total()
            .zip(labels_before)
            .map(|(a, b)| a.saturating_sub(b))
    );
    Ok(())
}

async fn cancel(fixture: &Fixture) -> CaseResult {
    let process = "l3_cancel";
    let people = published(fixture, process).await?;
    let started = start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({}),
    )
    .await?;
    started.expect(201)?;
    let run_id = started.field("id").as_str().unwrap_or_default().to_owned();
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();

    // A user who may not see the run is told it does not exist; one who sees it but may not cancel
    // it is told so.
    let unseen = people
        .unrelated
        .send_json(
            "POST",
            &format!("/api/v1/process/runs/{run_id}/cancel"),
            &[],
            &json!({"reason": "not mine"}),
        )
        .await?;
    unseen.expect_error("process.run_not_found")?;
    let refused = people
        .approver
        .send_json(
            "POST",
            &format!("/api/v1/process/runs/{run_id}/cancel"),
            &[],
            &json!({"reason": "only the starter or a supervisor may"}),
        )
        .await?;
    ensure!(
        refused.status == 403 && refused.error_code().as_deref() == Some("permission.denied"),
        "an assignee who is not the starter cancelling: HTTP {} {}",
        refused.status,
        refused.text()
    );

    // The starter cancels; the task closes; a repeat is not active.
    let cancelled = people
        .requester
        .send_json(
            "POST",
            &format!("/api/v1/process/runs/{run_id}/cancel"),
            &[],
            &json!({"reason": "Entered for the wrong customer."}),
        )
        .await?;
    cancelled.expect(200)?;
    ensure!(
        cancelled.field("state") == json!("cancelled"),
        "the cancelled run is not `cancelled`: {}",
        cancelled.text()
    );
    ensure!(
        inbox(&people.approver, process).await?.is_empty(),
        "the task of a cancelled run is still in the approver's inbox"
    );
    let gone = people
        .approver
        .get(&format!("/api/v1/task/{task_id}"))
        .await?;
    ensure!(
        gone.status == 404 || gone.field("task")["state"] == json!("cancelled"),
        "the task of a cancelled run is still actionable: {}",
        gone.text()
    );
    let again = people
        .requester
        .send_json(
            "POST",
            &format!("/api/v1/process/runs/{run_id}/cancel"),
            &[],
            &json!({"reason": "twice"}),
        )
        .await?;
    again.expect_error("process.run_not_active")?;
    Ok(())
}

async fn versions(fixture: &Fixture) -> CaseResult {
    let process = "l3_versions";
    let people = published(fixture, process).await?;
    let admin = fixture.admin();
    let started = start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({}),
    )
    .await?;
    started.expect(201)?;
    let old_run = started.field("id").as_str().unwrap_or_default().to_owned();
    ensure!(
        started.field("version") == json!(1),
        "the first run is not on v1: {}",
        started.text()
    );
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();

    // Version 2 while a run is on version 1.
    let v2 = publish(
        &admin,
        process,
        &definition(process, "Approve the request (second version)"),
        &fixture.unique("pub2"),
    )
    .await?;
    v2.expect(201)?;
    ensure!(
        v2.field("version") == json!(2),
        "the second publish is not v2: {}",
        v2.text()
    );
    let listed = admin
        .get(&format!("/api/v1/process/{process}/versions"))
        .await?;
    listed.expect(200)?;
    let rows = listed.rows()?;
    ensure!(
        rows.len() == 2,
        "the version list has {} entries: {}",
        rows.len(),
        listed.text()
    );
    ensure!(
        rows.iter()
            .any(|v| v["version"] == json!(1) && v["running_runs"] == json!(1)),
        "v1 does not show its running run: {}",
        listed.text()
    );

    // The old run finishes on v1; a new run starts on v2.
    let decided = decide(
        &people.approver,
        &task_id,
        "approve",
        &fixture.unique("decide"),
    )
    .await?;
    decided.expect(200)?;
    let finished = wait_for_state(&people.requester, &old_run, "completed").await?;
    ensure!(
        finished["version"] == json!(1),
        "the old run moved off v1: {finished}"
    );
    let fresh = start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({}),
    )
    .await?;
    fresh.expect(201)?;
    ensure!(
        fresh.field("version") == json!(2),
        "a new run is not on v2: {}",
        fresh.text()
    );

    // A migration plan is a dry run: it answers, and changes no run.
    let plan = admin
        .send_json(
            "POST",
            &format!("/api/v1/process/{process}/versions/1/migrations/plan"),
            &[],
            &json!({"to": 2}),
        )
        .await?;
    ensure!(
        plan.status == 200 || plan.status == 422,
        "the migration plan answers HTTP {}: {}",
        plan.status,
        plan.text()
    );
    let after = run(
        &people.requester,
        fresh.field("id").as_str().unwrap_or_default(),
    )
    .await?;
    after.expect(200)?;
    ensure!(
        after.field("version") == json!(2),
        "a dry-run plan changed a run's version: {}",
        after.text()
    );
    Ok(())
}

async fn webhook(fixture: &Fixture) -> CaseResult {
    let process = "l3_hook";
    let people = published(fixture, process).await?;
    let anonymous = fixture.admin().anonymous();
    let body = json!({"amount": "1"});
    let mut answers = Vec::new();
    for (trigger, signature) in [
        ("l3-hook", None),
        (
            "l3-hook",
            Some("t=1,v1=0000000000000000000000000000000000000000000000000000000000000000"),
        ),
        ("no-such-trigger", Some("t=1,v1=00")),
    ] {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(signature) = signature {
            headers.push(("X-Spot-Signature", signature));
        }
        let reply = anonymous
            .send_json(
                "POST",
                &format!("/api/v1/process/hook/{trigger}"),
                &headers,
                &body,
            )
            .await?;
        reply.expect_error("process.webhook_rejected")?;
        ensure!(
            reply.status == 401,
            "a rejected delivery answers HTTP {}",
            reply.status
        );
        answers.push(reply);
    }
    for pair in answers.windows(2) {
        let normal = |r: &crate::client::Reply| {
            let mut json = r.json();
            if let Some(object) = json.as_object_mut() {
                object.remove("request_id");
                if let Some(error) = object.get_mut("error").and_then(|e| e.as_object_mut()) {
                    error.remove("request_id");
                }
            }
            json
        };
        ensure!(
            normal(&pair[0]) == normal(&pair[1]),
            "two rejections differ: {} / {}",
            pair[0].text(),
            pair[1].text()
        );
    }
    let runs = people
        .requester
        .get(&format!("/api/v1/process/runs?process={process}"))
        .await?;
    runs.expect(200)?;
    ensure!(
        runs.rows()?.is_empty(),
        "a rejected delivery started a run: {}",
        runs.text()
    );
    Ok(())
}
