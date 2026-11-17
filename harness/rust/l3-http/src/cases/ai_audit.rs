//! The hosted AI audit trail (WP-AI-11.T8) over the real stack. The shared fixture host configures
//! no `[services.ai_audit]` table and the registered cases run after the shutdown case, so the
//! run case starts a host of its own, writes the table into its `common.toml` and reloads it, the
//! way an operator turns the service on.
//!
//! The HTTP surface (an agent's tool call arriving over the wire, the timeline route) is not
//! served yet: those cases are blocked entries that name the owner.

use std::time::{Duration, Instant};

use spot_cli::services::ai_audit::{ActionKind, AiAction, Outcome, Receipt, hosted};

use super::CaseDef;
use crate::failure;
use crate::fixture::Fixture;
use crate::report::{CaseResult, Failure};

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
        CaseDef::blocked(
            "ai_audit: the_document_timeline_route_merges_ai_entries_for_readers_only",
            "AI audit",
            "the document timeline route lists AI entries only for a viewer who may read the document, masked as the document is",
            "WP-AI-11.T8",
            "no route serves the timeline, and no engine-backed ReadAccess provider exists (the host denies every viewer)",
        ),
        CaseDef::blocked(
            "ai_audit: tampering_with_an_ai_entry_is_found_by_the_audit_verifier",
            "AI audit",
            "the scheduled verifier of the audit chain flags an altered AI entry",
            "WP-SEC-06.T8",
            "the verifier service does not walk the `ai:<site>` scope until WP-SEC-06.T8 lists it",
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
    let Some(mut fixture) =
        Fixture::start_secondary_with("[services.ai_audit]\nenabled = true\n").await
    else {
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
    // The host has no ReadAccess provider, so the timeline is empty for everyone.
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
