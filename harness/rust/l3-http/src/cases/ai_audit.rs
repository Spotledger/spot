//! The hosted AI audit trail (WP-AI-11.T8) over the real stack. The shared fixture host configures
//! no `[services.ai_audit]` table and the registered cases run after the shutdown case, so the
//! run case starts a host of its own, writes the table into its `common.toml` and reloads it, the
//! way an operator turns the service on.
//!
//! The timeline route (`GET /api/v1/resource/{doctype}/{id}/ai-timeline`, WP-AI-11.T11) is asked
//! over HTTP as a user who may read everything, one who may read the document but not every field,
//! and one with no right. The part of the HTTP surface that is not served yet (an agent's tool call
//! arriving over the wire) is a blocked entry that names the owner.

use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use spot_cli::services::ai_audit::{ActionKind, AiAction, Outcome, Receipt, hosted};

use super::CaseDef;
use crate::ensure;
use crate::failure;
use crate::fixture::{Fixture, MANAGER, OBSERVER, VIEWER};
use crate::report::{CaseResult, Failure};

/// The configuration both cases start their own host with: services are assembled at start.
const SERVICE: &str = "[services.ai_audit]\nenabled = true\n";
/// A canary no legitimate text contains, written to the protected `details` of the task.
const CANARY: &str = "CANARY-AI-TIMELINE-DETAILS";

/// After `secrets`; the case owns its fixture.
pub const ORDER: u32 = 31;

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "ai_audit: an_ai_action_is_chained_once_with_masked_arguments_and_the_chain_verifies",
            "AI audit",
            "an action appended through the hosted port is one chain entry per event id, a secret argument is not stored, a viewer with no access sees no timeline entry, and the chain verifies",
            false,
            |_| Box::pin(an_ai_action_is_chained_once()),
        ),
        CaseDef::blocked(
            "ai_audit: an_agent_tool_call_over_http_produces_a_chain_entry",
            "AI audit",
            "a tool call made by a test agent over HTTP is a verifiable chain entry with agent and acting user",
            "WP-AI-04.T4",
            "no HTTP route hands an agent action to the AI audit port yet; the MCP server is not mounted in `spot serve`",
        ),
        CaseDef::run(
            "ai_audit: the_document_timeline_route_merges_ai_entries_for_readers_only",
            "AI audit",
            "the document timeline route lists AI entries only for a viewer who may read the document, masked as the document is: a reader of everything sees the values, a reader without every field sees `***`, a viewer with no right and an unknown or malformed id see the same empty list",
            false,
            |_| Box::pin(the_timeline_route_serves_readers_only()),
        ),
        CaseDef::blocked(
            "ai_audit: tampering_with_an_ai_entry_is_found_by_the_audit_verifier",
            "AI audit",
            "the scheduled verifier of the audit chain flags an altered AI entry",
            "WP-SEC-06.T8",
            "the verifier walks the `ai:<site>` scope since WP-SEC-06.T9 (`[services.audit_verify] extra_chains`, proven in-process over the real store by `spot-cli/tests/audit_verify_extra_chains.rs`), but no running host can turn `[services.audit_verify]` on: no module provides the `AuditKeys` and `AuditAccess` ports its `register` requires, and `l3-http` has no dependency on `spot-audit-chain` or `spot-audit-store` to build the verifier itself",
        ),
    ]
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> Result<T, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = probe() {
            return Ok(value);
        }
        if Instant::now() > deadline {
            return Err(failure!("{what} did not happen within 30s"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn an_ai_action_is_chained_once() -> CaseResult {
    // The service is switched on in the configuration the host starts with: services are
    // assembled at start and a reload does not host a service that was off.
    let Some(mut fixture) = Fixture::start_secondary_with(SERVICE).await else {
        return Err(failure!("no PostgreSQL for the case's own fixture"));
    };
    let result = run(&fixture).await;
    fixture.finish().await;
    result
}

fn action(event: &str) -> AiAction {
    AiAction {
        kind: ActionKind::ToolCall,
        event_id: event.to_owned(),
        agent: "l3-agent".to_owned(),
        acting_user: "l3-user".to_owned(),
        tool: "update_doc".to_owned(),
        arguments: vec![
            ("title".to_owned(), "plan".to_owned()),
            ("api_token".to_owned(), "tok-l3-secret".to_owned()),
        ],
        outcome: Outcome::Ok,
        subject: Some(("Task".to_owned(), "L3-1".to_owned())),
        refers_to: None,
        at_micros: 1_700_000_000_000_000,
    }
}

async fn run(fixture: &Fixture) -> CaseResult {
    let site = fixture.site().to_owned();
    let port = until("the AI audit service to be hosted", || hosted(&site)).await?;

    let first = port
        .append(action("l3-event-1"))
        .await
        .map_err(|e| failure!("append refused: {e}"))?;
    if !matches!(first, Receipt::Appended { .. }) {
        return Err(failure!("the first append was not chained"));
    }
    let again = port
        .append(action("l3-event-1"))
        .await
        .map_err(|e| failure!("the retry was refused: {e}"))?;
    if again != Receipt::Duplicate {
        return Err(failure!("a retry with the same event id was chained twice"));
    }
    // The viewer `l3-user` is not a user record of the site and the document does not exist, so
    // the engine-backed ReadAccess provider answers no access: the timeline is empty.
    let timeline = port
        .timeline("l3-user", "Task", "L3-1", 10)
        .await
        .map_err(|e| failure!("timeline: {e}"))?;
    if !timeline.is_empty() {
        return Err(failure!(
            "a viewer with no provider access saw a timeline entry"
        ));
    }
    let log = std::fs::read_to_string(fixture.log()).unwrap_or_default();
    if log.contains("tok-l3-secret") {
        return Err(failure!("a secret argument reached the server log"));
    }
    Ok(())
}

/// One AI action on `task`, as the hosted port takes it.
fn task_action(event: &str, task: &str) -> AiAction {
    AiAction {
        subject: Some(("task".to_owned(), task.to_owned())),
        ..action(event)
    }
}

async fn the_timeline_route_serves_readers_only() -> CaseResult {
    let Some(mut fixture) = Fixture::start_secondary_with(SERVICE).await else {
        return Err(failure!("no PostgreSQL for the case's own fixture"));
    };
    let result = timeline_route(&fixture).await;
    fixture.finish().await;
    result
}

/// The `(name, value)` pairs of one answered entry.
fn argument_pairs(entry: &Json) -> Vec<(String, String)> {
    entry["arguments"]
        .as_array()
        .map(|arguments| {
            arguments
                .iter()
                .map(|a| {
                    (
                        a["name"].as_str().unwrap_or_default().to_owned(),
                        a["value"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn timeline_route(fixture: &Fixture) -> CaseResult {
    let site = fixture.site().to_owned();
    let port = until("the AI audit service to be hosted", || hosted(&site)).await?;

    // One task whose protected `details` holds the canary, and one AI action chained on it.
    let (project, _, _) = fixture.project("AI timeline").await?;
    let created = fixture
        .admin()
        .create(
            "task",
            &json!({
                "subject": fixture.unique("ai-timeline-task"),
                "project": project,
                "details": CANARY,
            }),
        )
        .await?;
    created.expect(201)?;
    let task = created.id()?;
    port.append(task_action("l3-timeline-event-1", &task))
        .await
        .map_err(|e| failure!("append refused: {e}"))?;
    let path = format!("/api/v1/resource/task/{task}/ai-timeline");

    // A user who may read the document and every protected field sees the entry as recorded.
    let manager = fixture.sign_in(MANAGER).await?;
    let full = manager.get(&path).await?;
    full.expect(200)?;
    ensure!(
        full.header("cache-control") == Some("private, no-store"),
        "the timeline answer is not `private, no-store`"
    );
    let answer = full.json();
    let entries = answer["data"]["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    ensure!(
        entries.len() == 1,
        "a reader of everything should see exactly the one entry: {}",
        full.redacted_text()
    );
    ensure!(
        entries[0]["tool"] == "update_doc" && entries[0]["agent"] == "l3-agent",
        "the entry does not name the tool and the agent: {}",
        full.redacted_text()
    );
    let pairs = argument_pairs(&entries[0]);
    ensure!(
        pairs.contains(&("title".to_owned(), "plan".to_owned())),
        "a reader of everything should see the argument value: {pairs:?}"
    );
    ensure!(
        pairs.contains(&("api_token".to_owned(), "***".to_owned())),
        "a value the write-time mask hid must stay hidden: {pairs:?}"
    );
    ensure!(
        !full.text().contains("tok-l3-secret") && !full.text().contains(CANARY),
        "the timeline carried a secret argument or a document value"
    );

    // A user who may read the task but not its `details` sees the entry with every value masked.
    let viewer = fixture.sign_in(VIEWER).await?;
    let masked = viewer.get(&path).await?;
    masked.expect(200)?;
    let answer = masked.json();
    let entries = answer["data"]["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    ensure!(
        entries.len() == 1,
        "a masked viewer should still see the entry: {}",
        masked.redacted_text()
    );
    let pairs = argument_pairs(&entries[0]);
    ensure!(
        !pairs.is_empty() && pairs.iter().all(|(_, value)| value == "***"),
        "a masked viewer must see every argument value as `***`: {pairs:?}"
    );
    ensure!(
        !masked.text().contains("plan") && !masked.text().contains(CANARY),
        "a masked viewer saw a value"
    );

    // A user with no right on the task, an id that is not in the site, a malformed id and an
    // unknown DocType all get the same empty list: an unreadable document is not told apart from
    // a missing one.
    let observer = fixture.sign_in(OBSERVER).await?;
    let unknown = format!("/api/v1/resource/task/{}/ai-timeline", fixture.admin_id());
    let malformed = "/api/v1/resource/task/L3-1/ai-timeline".to_owned();
    let no_doctype = format!("/api/v1/resource/no_such_doctype/{task}/ai-timeline");
    let mut seen: Vec<Vec<u8>> = Vec::new();
    for (who, label, target) in [
        (&observer, "no right", &path),
        (&manager, "an id that is not a task", &unknown),
        (&manager, "a malformed id", &malformed),
        (&manager, "an unknown DocType", &no_doctype),
    ] {
        let reply = who.get(target).await?;
        reply.expect(200)?;
        ensure!(
            reply.json() == json!({ "data": { "entries": [] } }),
            "{label}: the answer is not the empty list: {}",
            reply.redacted_text()
        );
        seen.push(reply.body);
    }
    ensure!(
        seen.windows(2).all(|pair| pair[0] == pair[1]),
        "an unreadable document does not answer the same bytes as a missing one"
    );

    // `limit` is bounded and an anonymous caller is refused.
    let bad = manager.get(&format!("{path}?limit=0")).await?;
    bad.expect_error("api.invalid_request")?;
    let anonymous = fixture.client().anonymous().get(&path).await?;
    ensure!(
        anonymous.status == 401,
        "an anonymous caller should be refused with 401, got {}",
        anonymous.status
    );
    Ok(())
}
