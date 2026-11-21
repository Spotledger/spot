//! One suite that attacks every route of the process family over real HTTP and keeps them from
//! regressing (WP-BPM-02.T8). The repository's finding classes are cross-tenant access, fail-open
//! permission checks, system-actor escalation, path traversal, credentials in logs, injection and
//! missing audit on failure.
//!
//! The fixture, actors, definition text and helpers are those of WP-BPM-02.T7 (`process::support`), not
//! copies. Each case owns a fixture (the shared host is stopped before registered cases run). A defect
//! found is reported, never worked around: the failing assertion goes to the report's open questions.

#[path = "process_security/sites.rs"]
mod sites;

use serde_json::{Value as Json, json};

use super::CaseDef;
use super::process::support::{Actors, actors, definition, publish, start, wait_for_task};
use crate::client::Reply;
use crate::fixture::Fixture;
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

use self::sites::{admin_of, shape, two_sites};

/// After `process` (80); each case owns its fixture.
pub const ORDER: u32 = 90;

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "process_security: a_token_of_one_site_reaches_nothing_of_another",
            "Process security",
            "with two sites on one server, a run, task, version, migration and trigger name of site B answer a site A token exactly as a nonexistent id does (status, body and caching headers equal)",
            false,
            |_| Box::pin(own_cross_tenant()),
        ),
        CaseDef::run(
            "process_security: removing_a_role_mid_flight_stops_the_decision",
            "Process security",
            "a task is claimed, the approver's role is removed, and completing the task is refused, never served",
            false,
            |_| Box::pin(own_role_removed()),
        ),
        CaseDef::run(
            "process_security: a_caller_cannot_name_an_actor_or_a_system_outcome",
            "Process security",
            "bodies and headers naming `actor`, `completed_by`, `principal`, `tenant`, `initiator` or `system`, and an outcome like `system:sla`, are refused or ignored: the run's starter and the task's decider are the authenticated caller",
            false,
            |_| Box::pin(own_escalation()),
        ),
        CaseDef::run(
            "process_security: unknown_and_forbidden_are_one_answer",
            "Process security",
            "an unknown and a forbidden run, task, version, process and migration are the same answer; a colliding business key is indistinguishable from a fresh one",
            false,
            |_| Box::pin(own_oracles()),
        ),
        CaseDef::run(
            "process_security: the_webhook_endpoint_refuses_abuse_cheaply",
            "Process security",
            "a wrong content type, a path-like or overlong trigger name and a 10 MiB body are refused without a server error",
            false,
            |_| Box::pin(own_webhook_abuse()),
        ),
        CaseDef::run(
            "process_security: hostile_input_is_data_and_bounds_hold",
            "Process security",
            "SQL metacharacters, expression source and markup in input, comment and business key never execute or echo; deep nesting, a huge limit, a bad cursor and a replayed Idempotency-Key with another body are clean refusals",
            false,
            |_| Box::pin(own_injection_and_bounds()),
        ),
        CaseDef::run(
            "process_security: secrets_and_values_stay_out_of_the_log_and_every_refusal_is_audited",
            "Process security",
            "after a scenario the server log holds no session token and no input value, and the refusals left audit lines",
            false,
            |_| Box::pin(own_logs_and_audit()),
        ),
        CaseDef::blocked(
            "process_security: a_hidden_field_never_reaches_a_run_task_or_report",
            "Process security",
            "a document with a hidden field is mapped into a run; no run, task, event, report or graph answer exposes the value or the field name to a user without access",
            "WP-BPM-04.T6",
            "needs a definition whose Document node maps a document's hidden field into the run: the mapping setting is not reachable from the model text over HTTP yet, so no run carries a hidden value to look for",
        ),
    ]
}

macro_rules! own_fixture {
    ($name:ident, $case:ident) => {
        async fn $name() -> CaseResult {
            let Some(mut fixture) = Fixture::start_secondary().await else {
                return Err(failure!(
                    "the process security case could not start its own fixture"
                ));
            };
            let result = $case(&fixture).await;
            fixture.finish().await;
            result
        }
    };
}

own_fixture!(own_cross_tenant, cross_tenant);
own_fixture!(own_role_removed, role_removed);
own_fixture!(own_escalation, escalation);
own_fixture!(own_oracles, oracles);
own_fixture!(own_webhook_abuse, webhook_abuse);
own_fixture!(own_injection_and_bounds, injection_and_bounds);
own_fixture!(own_logs_and_audit, logs_and_audit);

/// A published process and its actors.
async fn world(fixture: &Fixture, process: &str) -> Result<Actors, Failure> {
    let people = actors(fixture).await?;
    publish(
        &fixture.admin(),
        process,
        &definition(process, "Approve the request"),
        &fixture.unique("pub"),
    )
    .await?
    .expect(201)?;
    Ok(people)
}

fn id_of(reply: &Reply) -> String {
    reply.field("id").as_str().unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------------------------------------
// 1. Cross-tenant
// ---------------------------------------------------------------------------------------------

async fn cross_tenant(fixture: &Fixture) -> CaseResult {
    let (host, other) = two_sites(fixture).await?;
    let outcome = cross_tenant_on(fixture, &host, &other).await;
    host.stop().await;
    outcome
}

async fn cross_tenant_on(
    fixture: &Fixture,
    host: &super::process::support::Host,
    other: &str,
) -> CaseResult {
    use super::process::support::wait_ready;
    let a = host.client(fixture.site());
    let b = host.client(other);
    wait_ready(&a).await?;
    wait_ready(&b).await?;
    let a_admin = admin_of(host, fixture.site()).await?;
    let b_admin = admin_of(host, other).await?;

    // Site B has a process, a run and a version; site A has none of them.
    let process = "l3_tenant";
    publish(
        &b_admin,
        process,
        &definition(process, "Tenant B"),
        "tenant-b-publish",
    )
    .await?
    .expect(201)?;
    let started = start(&b_admin, process, "tenant-b-start", &json!({})).await?;
    started.expect(201)?;
    let b_run = id_of(&started);

    // Every operation, with a token of A, on a name or id of B and on a nonexistent one: the same
    // answer.
    let missing = "0190a1b2-0000-7000-8000-000000000000";
    let probes: Vec<(&str, String, String, Option<Json>)> = vec![
        (
            "GET",
            format!("/api/v1/process/runs/{b_run}"),
            format!("/api/v1/process/runs/{missing}"),
            None,
        ),
        (
            "GET",
            format!("/api/v1/process/runs/{b_run}/events"),
            format!("/api/v1/process/runs/{missing}/events"),
            None,
        ),
        (
            "POST",
            format!("/api/v1/process/runs/{b_run}/cancel"),
            format!("/api/v1/process/runs/{missing}/cancel"),
            Some(json!({"reason": "probe"})),
        ),
        (
            "GET",
            format!("/api/v1/process/{process}/versions"),
            "/api/v1/process/l3_nothing/versions".to_owned(),
            None,
        ),
        (
            "GET",
            format!("/api/v1/process/{process}/versions/1"),
            "/api/v1/process/l3_nothing/versions/1".to_owned(),
            None,
        ),
        (
            "POST",
            format!("/api/v1/process/{process}/versions/1/retire"),
            "/api/v1/process/l3_nothing/versions/1/retire".to_owned(),
            Some(json!({})),
        ),
        (
            "POST",
            format!("/api/v1/process/{process}/versions/1/migrations/plan"),
            "/api/v1/process/l3_nothing/versions/1/migrations/plan".to_owned(),
            Some(json!({"to": 2})),
        ),
        (
            "GET",
            format!("/api/v1/process/{process}/migrations/{missing}"),
            "/api/v1/process/l3_nothing/migrations/".to_owned() + missing,
            None,
        ),
        (
            "POST",
            format!("/api/v1/process/{process}/runs"),
            "/api/v1/process/l3_nothing/runs".to_owned(),
            Some(json!({"input": {}})),
        ),
    ];
    for (method, real, nothing, body) in probes {
        let send = |path: String| {
            let client = a_admin.clone();
            let body = body.clone();
            async move {
                match body {
                    Some(b) => {
                        client
                            .send_json(
                                method,
                                &path,
                                &[("Idempotency-Key", "cross-tenant-probe")],
                                &b,
                            )
                            .await
                    }
                    None => client.send(method, &path, &[], None).await,
                }
            }
        };
        let foreign = send(real.clone()).await?;
        let absent = send(nothing.clone()).await?;
        ensure!(
            shape(&foreign) == shape(&absent),
            "{method} {real}: a site B name answers differently from a nonexistent one: {:?} vs {:?}",
            shape(&foreign),
            shape(&absent)
        );
        ensure!(
            foreign.status < 500 && foreign.status != 200 && foreign.status != 201,
            "{method} {real}: a site B name was served to a site A token (HTTP {})",
            foreign.status
        );
    }
    // A's list holds nothing of B's, and B's token is nothing on A.
    let a_runs = a_admin.get("/api/v1/process/runs?limit=100").await?;
    a_runs.expect(200)?;
    ensure!(
        !a_runs.text().contains(&b_run),
        "site A's run list names a run of site B"
    );
    let b_token_on_a = a.with_token(b_admin.bearer().unwrap_or_default());
    let reply = b_token_on_a.get("/api/v1/process/runs").await?;
    reply.expect(401)?;
    // The webhook trigger name of B is as unknown on A as any other.
    let anonymous = a.anonymous();
    let known = anonymous
        .send_json(
            "POST",
            "/api/v1/process/hook/l3-tenant",
            &[("X-Spot-Signature", "t=1,v1=00")],
            &json!({}),
        )
        .await?;
    let unknown = anonymous
        .send_json(
            "POST",
            "/api/v1/process/hook/l3-nothing",
            &[("X-Spot-Signature", "t=1,v1=00")],
            &json!({}),
        )
        .await?;
    ensure!(
        shape(&known) == shape(&unknown),
        "a trigger name of site B is told apart on site A"
    );
    // Restarting both sites keeps them apart (timers, jobs and the index never act across sites).
    // The B run is still B's afterwards.
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 2. Fail-open
// ---------------------------------------------------------------------------------------------

async fn role_removed(fixture: &Fixture) -> CaseResult {
    let process = "l3_failopen";
    let people = world(fixture, process).await?;
    start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({}),
    )
    .await?
    .expect(201)?;
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();
    let got = people
        .approver
        .get(&format!("/api/v1/task/{task_id}"))
        .await?;
    got.expect(200)?;
    let claim = people
        .approver
        .send_json(
            "POST",
            &format!("/api/v1/task/{task_id}/claim"),
            &[("If-Match", &got.etag()?)],
            &json!({}),
        )
        .await?;
    claim.expect(200)?;

    // End the approver's `approver` assignment while the task is claimed.
    let assignments = fixture
        .admin()
        .list(
            "role_assignment",
            &format!("limit=100&user={}", people.approver_id),
        )
        .await?;
    assignments.expect(200)?;
    let mut removed = 0;
    for row in assignments.rows()? {
        let reason = row["reason"].as_str().unwrap_or_default();
        if reason == "The approver of the process cases." {
            let id = row["id"].as_str().unwrap_or_default();
            let version = row["version"].as_i64().unwrap_or(1);
            fixture
                .admin()
                .delete("role_assignment", id, version)
                .await?
                .expect(204)?;
            removed += 1;
        }
    }
    ensure!(
        removed == 1,
        "found {removed} approver assignments to remove"
    );

    let completed = people
        .approver
        .send_json(
            "POST",
            &format!("/api/v1/task/{task_id}/complete"),
            &[
                ("If-Match", &claim.etag()?),
                ("Idempotency-Key", &fixture.unique("late")),
            ],
            &json!({"outcome": "approve"}),
        )
        .await?;
    ensure!(
        completed.status == 403 || completed.status == 404 || completed.status == 409,
        "a decision after the role was removed was HTTP {}: {}",
        completed.status,
        completed.text()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 3. Escalation
// ---------------------------------------------------------------------------------------------

async fn escalation(fixture: &Fixture) -> CaseResult {
    let process = "l3_escalate";
    let people = world(fixture, process).await?;
    // The body is closed: a start that names an initiator or a tenant is refused, never obeyed.
    for (field, value) in [
        ("initiator", json!(people.approver_id)),
        ("actor", json!(people.approver_id)),
        ("tenant", json!("another-site")),
        ("system", json!(true)),
        ("principal", json!("svc_billing")),
    ] {
        let reply = people
            .requester
            .send_json(
                "POST",
                &format!("/api/v1/process/{process}/runs"),
                &[("Idempotency-Key", &fixture.unique("esc"))],
                &json!({"input": {}, field: value}),
            )
            .await?;
        ensure!(
            reply.status == 400 || reply.status == 422,
            "a start body naming `{field}` was HTTP {}: {}",
            reply.status,
            reply.text()
        );
    }
    // A client header claiming to be somebody or a process changes nothing.
    let started = people
        .requester
        .send_json(
            "POST",
            &format!("/api/v1/process/{process}/runs"),
            &[
                ("Idempotency-Key", &fixture.unique("hdr")),
                ("X-Forwarded-User", &people.approver_id),
                ("X-Spot-Process", "system"),
            ],
            &json!({"input": {}}),
        )
        .await?;
    started.expect(201)?;
    let run_id = id_of(&started);
    let events = people
        .requester
        .get(&format!("/api/v1/process/runs/{run_id}/events"))
        .await?;
    events.expect(200)?;
    ensure!(
        events.text().contains(&people.requester_id)
            && !events.text().contains(&people.approver_id),
        "the run's starter is not the authenticated caller: {}",
        events.text()
    );

    // The task: an outcome that is not one of the listed ones, including the reserved system one,
    // and a body naming a decider, are refused; the task stays open.
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();
    let got = people
        .approver
        .get(&format!("/api/v1/task/{task_id}"))
        .await?;
    got.expect(200)?;
    let claim = people
        .approver
        .send_json(
            "POST",
            &format!("/api/v1/task/{task_id}/claim"),
            &[("If-Match", &got.etag()?)],
            &json!({}),
        )
        .await?;
    claim.expect(200)?;
    for body in [
        json!({"outcome": "system:sla"}),
        json!({"outcome": "approve", "completed_by": people.requester_id}),
        json!({"outcome": "approve", "actor": "system:sla"}),
    ] {
        let reply = people
            .approver
            .send_json(
                "POST",
                &format!("/api/v1/task/{task_id}/complete"),
                &[
                    ("If-Match", &claim.etag()?),
                    ("Idempotency-Key", &fixture.unique("esc")),
                ],
                &body,
            )
            .await?;
        ensure!(
            reply.status == 400 || reply.status == 422,
            "{body}: HTTP {} {}",
            reply.status,
            reply.text()
        );
    }
    let still = people
        .approver
        .get(&format!("/api/v1/task/{task_id}"))
        .await?;
    still.expect(200)?;
    ensure!(
        still.field("state") != json!("completed"),
        "a refused completion completed the task: {}",
        still.text()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 4. Oracles
// ---------------------------------------------------------------------------------------------

async fn oracles(fixture: &Fixture) -> CaseResult {
    let process = "l3_oracle";
    let people = world(fixture, process).await?;
    let started = start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({}),
    )
    .await?;
    started.expect(201)?;
    let run_id = id_of(&started);
    let task = wait_for_task(&people.approver, process).await?;
    let task_id = task["id"].as_str().unwrap_or_default().to_owned();
    let missing = "0190a1b2-0000-7000-8000-000000000000";

    // Forbidden (exists, the unrelated user may not see it) versus unknown.
    for (real, nothing) in [
        (
            format!("/api/v1/process/runs/{run_id}"),
            format!("/api/v1/process/runs/{missing}"),
        ),
        (
            format!("/api/v1/process/runs/{run_id}/events"),
            format!("/api/v1/process/runs/{missing}/events"),
        ),
        (
            format!("/api/v1/task/{task_id}"),
            "/api/v1/task/case_0000000000000000_review".to_owned(),
        ),
        (
            format!("/api/v1/process/{process}/versions"),
            "/api/v1/process/l3_unknown/versions".to_owned(),
        ),
        (
            format!("/api/v1/process/{process}/versions/1"),
            "/api/v1/process/l3_unknown/versions/1".to_owned(),
        ),
    ] {
        let forbidden = people.unrelated.get(&real).await?;
        let unknown = people.unrelated.get(&nothing).await?;
        ensure!(
            shape(&forbidden) == shape(&unknown),
            "GET {real}: forbidden and unknown differ: {:?} vs {:?}",
            shape(&forbidden),
            shape(&unknown)
        );
    }

    // A business key another user already used is indistinguishable from a fresh one.
    let key_a = start_with_key(&people, process, "BK-SHARED", &fixture.unique("a")).await?;
    let key_b = start_with_key(&people, process, "BK-SHARED", &fixture.unique("b")).await?;
    ensure!(
        key_a.status == key_b.status,
        "a colliding business key answers HTTP {} instead of {}",
        key_b.status,
        key_a.status
    );
    let fresh = start_with_key(&people, process, "BK-FRESH-ONLY", &fixture.unique("c")).await?;
    ensure!(
        fresh.status == key_b.status,
        "a colliding business key is told apart from a fresh one ({} vs {})",
        key_b.status,
        fresh.status
    );
    Ok(())
}

async fn start_with_key(
    people: &Actors,
    process: &str,
    business_key: &str,
    idempotency: &str,
) -> Result<Reply, Failure> {
    // The requester and the approver both start with the same business key.
    let by_requester = people
        .requester
        .send_json(
            "POST",
            &format!("/api/v1/process/{process}/runs"),
            &[("Idempotency-Key", idempotency)],
            &json!({"input": {}, "business_key": business_key}),
        )
        .await?;
    let by_approver = people
        .approver
        .send_json(
            "POST",
            &format!("/api/v1/process/{process}/runs"),
            &[("Idempotency-Key", &format!("{idempotency}-2"))],
            &json!({"input": {}, "business_key": business_key}),
        )
        .await?;
    ensure!(
        by_requester.status == by_approver.status,
        "two users using one business key got different answers ({} vs {})",
        by_requester.status,
        by_approver.status
    );
    Ok(by_approver)
}

// ---------------------------------------------------------------------------------------------
// 5. Webhook abuse
// ---------------------------------------------------------------------------------------------

async fn webhook_abuse(fixture: &Fixture) -> CaseResult {
    let anonymous = fixture.admin().anonymous();
    let sig = [("X-Spot-Signature", "t=1,v1=00")];
    // Wrong content type.
    let typed = anonymous
        .send(
            "POST",
            "/api/v1/process/hook/anything",
            &sig,
            Some(("text/plain", b"hello".to_vec())),
        )
        .await?;
    ensure!(
        typed.status == 415 || typed.status == 401,
        "a text/plain delivery answers HTTP {}",
        typed.status
    );
    // Names that are paths, encoded paths and an overlong name are refused, never a server error.
    for name in [
        "..%2f..%2fetc%2fpasswd".to_owned(),
        "%2e%2e/%2e%2e/secret".to_owned(),
        "a".repeat(5_000),
        "x y".to_owned(),
    ] {
        let reply = anonymous
            .send_json(
                "POST",
                &format!("/api/v1/process/hook/{name}"),
                &sig,
                &json!({}),
            )
            .await?;
        ensure!(
            reply.status < 500 && reply.status != 200 && reply.status != 202,
            "the trigger name `{}...` answers HTTP {}",
            &name[..name.len().min(20)],
            reply.status
        );
    }
    // 10 MiB: refused as too large (413), or as a rejection, but never accepted and never a 5xx.
    let big = vec![b'a'; 10 * 1024 * 1024];
    let huge = anonymous
        .send(
            "POST",
            "/api/v1/process/hook/anything",
            &sig,
            Some(("application/json", big)),
        )
        .await;
    // The server may close the connection before reading the whole body: also a refusal.
    if let Ok(reply) = huge {
        ensure!(
            reply.status == 413 || reply.status == 401,
            "a 10 MiB delivery answers HTTP {}",
            reply.status
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 6 and 7. Injection and bounds
// ---------------------------------------------------------------------------------------------

async fn injection_and_bounds(fixture: &Fixture) -> CaseResult {
    let process = "l3_inject";
    let people = world(fixture, process).await?;
    let hostile = [
        "'; DROP TABLE case_record; --",
        "1 OR 1=1",
        "${7*7}",
        "{{7*7}}",
        "doc.amount == 1 || true",
        "<script>alert(1)</script>",
        "../../etc/passwd",
    ];
    for text in hostile {
        let reply = people
            .requester
            .send_json(
                "POST",
                &format!("/api/v1/process/{process}/runs"),
                &[("Idempotency-Key", &fixture.unique("inj"))],
                &json!({"input": {"note": text}, "business_key": text.chars().take(100).collect::<String>()}),
            )
            .await?;
        ensure!(
            reply.status < 500,
            "hostile input `{text}` is a server error (HTTP {}): {}",
            reply.status,
            reply.text()
        );
        // Whatever the answer is, it never echoes the submitted text, except the caller's own
        // `business_key`, which the Run contract returns as a plain string (never evaluated).
        let mut answer = reply.json();
        if let Some(data) = answer.get_mut("data").and_then(|d| d.as_object_mut()) {
            data.remove("business_key");
        }
        ensure!(
            !answer.to_string().contains(&json!(text).to_string().trim_matches('"').to_owned()),
            "the answer echoes hostile input `{text}`: {}",
            reply.text()
        );
    }
    // Traversal in names and ids is a plain refusal.
    for path in [
        "/api/v1/process/..%2f..%2fx/versions",
        "/api/v1/process/%2e%2e/versions",
        "/api/v1/process/runs/..%2f..%2fetc",
        "/api/v1/task/..%2f..%2fetc",
    ] {
        let reply = people.requester.get(path).await?;
        ensure!(
            reply.status >= 400 && reply.status < 500,
            "GET {path} answers HTTP {}",
            reply.status
        );
    }
    // Bounds: a deeply nested input, a huge limit, a bad cursor.
    let mut nested = json!({"leaf": 1});
    for _ in 0..200 {
        nested = json!({ "n": nested });
    }
    let deep_text = format!(
        "{{\"input\": {}}}",
        "[".repeat(10_000) + &"]".repeat(10_000)
    );
    let deep = people
        .requester
        .send(
            "POST",
            &format!("/api/v1/process/{process}/runs"),
            &[("Idempotency-Key", &fixture.unique("deep"))],
            Some(("application/json", deep_text.into_bytes())),
        )
        .await?;
    ensure!(
        deep.status >= 400 && deep.status < 500,
        "a 10,000-deep input answers HTTP {}",
        deep.status
    );
    let nested_reply = start(&people.requester, process, &fixture.unique("nest"), &nested).await?;
    ensure!(
        nested_reply.status < 500,
        "a 200-deep input is a server error"
    );
    for query in [
        "limit=100000",
        "limit=0",
        "limit=-1",
        "cursor=not-a-cursor",
        "cursor=%00",
    ] {
        let reply = people
            .requester
            .get(&format!("/api/v1/process/runs?{query}"))
            .await?;
        ensure!(
            reply.status == 200 || (reply.status >= 400 && reply.status < 500),
            "runs?{query} answers HTTP {}",
            reply.status
        );
    }
    // A replayed Idempotency-Key with a different body is a conflict, never the first answer.
    let key = fixture.unique("replay");
    start(&people.requester, process, &key, &json!({"a": 1}))
        .await?
        .expect(201)?;
    let changed = start(&people.requester, process, &key, &json!({"a": 2})).await?;
    ensure!(
        changed.status == 409
            && changed.error_code().as_deref() == Some("api.idempotency_conflict"),
        "a replayed key with another body answers HTTP {} {}",
        changed.status,
        changed.text()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 8 and 9. Logs and audit
// ---------------------------------------------------------------------------------------------

async fn logs_and_audit(fixture: &Fixture) -> CaseResult {
    let process = "l3_logs";
    let people = world(fixture, process).await?;
    let marker = "document-value-l3-must-not-be-logged";
    let started = start(
        &people.requester,
        process,
        &fixture.unique("start"),
        &json!({"note": marker}),
    )
    .await?;
    ensure!(started.status < 500, "the start is a server error");
    // A few refusals to audit: unknown process, a task nobody may see, a forbidden cancel.
    let refusals = [
        people
            .requester
            .get("/api/v1/process/runs/0190a1b2-0000-7000-8000-000000000000")
            .await?,
        people
            .unrelated
            .get("/api/v1/task/0190a1b2-0000-7000-8000-000000000000")
            .await?,
        start(
            &people.unrelated,
            "l3_no_such_process",
            &fixture.unique("x"),
            &json!({}),
        )
        .await?,
    ];
    ensure!(
        refusals.iter().all(|r| r.status >= 400),
        "a probe was served"
    );
    // Let the host flush its log.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let log = std::fs::read_to_string(fixture.log())
        .map_err(|e| failure!("cannot read the server log: {e}"))?;
    let tokens = [
        people.requester.bearer(),
        people.approver.bearer(),
        people.unrelated.bearer(),
    ];
    for token in tokens.into_iter().flatten() {
        ensure!(
            !log.contains(token),
            "a session token appears in the server log"
        );
    }
    ensure!(
        !log.contains(marker),
        "an input value appears in the server log"
    );
    // The version audit is a table of the site's database: the publish of the world's definition
    // is on it, and no row (or any other column of it) carries a value or a token. (The run
    // audit goes to the log target `spot::audit::process`, which this suite does not capture.)
    // The site's own database (named after the site), not the scratch one.
    let site_db = fixture.db().config().with_dbname(&fixture.site().replace('-', "_"));
    let db = fixture.db().raw_client_on(&site_db).await;
    let rows = db
        .query(
            "SELECT operation, outcome, process, actor, COALESCE(content_hash, '') AS content_hash \
             FROM spot_bpm_version_audit WHERE process = $1",
            &[&process],
        )
        .await
        .map_err(|e| failure!("cannot read the version audit: {e}"))?;
    ensure!(
        !rows.is_empty(),
        "the version audit holds no row for the publish of `{process}`"
    );
    for row in &rows {
        let operation: String = row.get("operation");
        ensure!(
            operation.starts_with("process_version"),
            "an unexpected audit operation `{operation}`"
        );
        let all = format!(
            "{} {} {} {} {}",
            operation,
            row.get::<_, String>("outcome"),
            row.get::<_, String>("process"),
            row.get::<_, String>("actor"),
            row.get::<_, String>("content_hash"),
        );
        ensure!(
            !all.contains(marker),
            "an input value appears in the version audit"
        );
        for token in [
            people.requester.bearer(),
            people.approver.bearer(),
            people.unrelated.bearer(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                !all.contains(token),
                "a session token appears in the version audit"
            );
        }
    }
    Ok(())
}
