//! The agent tools of the MCP service over the running server (WP-AI-04.T7): `retrieve`,
//! `recall_memory`, `remedy_list`, `remedy_dry_run`, `remedy_apply` and `remedy_undo`, offered by the
//! services that own them through the extension-tool seam (contract `mcp.md`, section 5).
//!
//! One case runs: the tools a site cannot serve are **absent** (not listed, and a call is the
//! same unknown-tool refusal as any other name), and the agent-only tools are absent for a user
//! principal. The cases that need an embedding provider, an agent principal or a remedy world stay
//! **blocked**, each naming exactly what is missing and the task that provides it; the behaviour
//! is proved in-process by `spot-cli/tests/agent_tools_mcp.rs` (the remedy tools over the real host),
//! `retrieval_service.rs`, `agent_memory_service.rs` and `spot-mcp/tests/extension_tools.rs`.
//!
//! A defect this suite finds is a defect to report, never a case to relax.

use serde_json::{Value as Json, json};

use super::CaseDef;
use crate::client::Client;
use crate::fixture::Fixture;
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the retrieval cases (75); each case owns its fixture.
pub const ORDER: u32 = 76;

const SECTION: &str = "Agent tools";

/// The six tool names of the contract.
const TOOLS: [&str; 6] = [
    "retrieve",
    "recall_memory",
    "remedy_list",
    "remedy_dry_run",
    "remedy_apply",
    "remedy_undo",
];

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "agent_tools: the_agent_tools_are_absent_for_a_user_and_while_their_services_are_off",
            SECTION,
            "a signed-in user's `tools/list` holds none of `retrieve`, `recall_memory` and the four `remedy_*` tools (retrieval and agent memory are off, the remedy tools are for agents), and `tools/call` of each is the same unknown-tool refusal as for any name nobody offers",
            false,
            |_| Box::pin(own_absent()),
        ),
        CaseDef::blocked(
            "agent_tools: a_t1_remedy_dry_runs_applies_and_undoes_through_the_tools",
            SECTION,
            "a registered agent calls `remedy_list`, `remedy_dry_run`, `remedy_apply` and `remedy_undo` over `POST /mcp`: the answer carries the sealed plan, the `undo_token`, `route` and `undo_expires_at`, a tampered plan is refused, and the audit chain holds the intent record before the change",
            "WP-AI-07.T5",
            "the `dyn WorldAccess` the composer provides (WP-AI-07.T5) has no part the harness site's remedies can run on (the seeded remedies need jobs and cache parts that read ports the harness does not configure), so a dry-run is refused by its precondition; and the harness has no path that signs in as a registered agent (the agent directory of WP-AI-06.T4 resolves one only from a stored registration)",
        ),
        CaseDef::blocked(
            "agent_tools: recall_memory_is_gated_by_the_agents_tier",
            SECTION,
            "a registered agent calls `recall_memory` and gets no entry learned above its tier, a user principal does not see the tool, and an entry carries no document field value",
            "WP-AI-03.T7",
            "the harness site configures no LLM tenant with an `embedding_model` (WP-AI-03.T7), so agent memory has no embedder, is degraded and `recall_memory` is not on tools/list; the in-process proof is `agent_memory_service.rs`",
        ),
    ]
}

async fn own_absent() -> CaseResult {
    let Some(mut fixture) = Fixture::start_secondary().await else {
        return Err(failure!(
            "the agent tools case could not start its own fixture"
        ));
    };
    let result = absent(&fixture).await;
    fixture.finish().await;
    result
}

/// The message of one JSON-RPC answer (the body is a single JSON message when no progress token
/// was sent).
fn message_of(text: &str) -> Json {
    serde_json::from_str(text).unwrap_or(Json::Null)
}

async fn rpc(client: &Client, id: u64, method: &str, params: Json) -> Result<Json, Failure> {
    let reply = client
        .send_json(
            "POST",
            "/mcp",
            &[],
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .await?;
    reply.expect(200)?;
    Ok(message_of(&reply.text()))
}

async fn absent(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let listed = rpc(&admin, 1, "tools/list", json!({})).await?;
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ensure!(
        names.iter().any(|name| name == "get_task"),
        "the administrator's tool list is empty or unreadable: {listed}"
    );
    for tool in TOOLS {
        ensure!(
            !names.iter().any(|name| name == tool),
            "`{tool}` is listed for a user principal: {names:?}"
        );
    }
    // A call to a tool nobody offers this principal is the unknown-tool refusal, whatever the
    // name: it does not say whether the tool exists for someone else.
    let nothing = rpc(
        &admin,
        2,
        "tools/call",
        json!({"name": "no_such_tool_at_all", "arguments": {}}),
    )
    .await?;
    let baseline = nothing["error"]["data"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ensure!(
        !baseline.is_empty(),
        "an unknown tool is not refused with a code: {nothing}"
    );
    for (n, tool) in TOOLS.iter().enumerate() {
        let answer = rpc(
            &admin,
            10 + n as u64,
            "tools/call",
            json!({"name": tool, "arguments": {"query": "anything"}}),
        )
        .await?;
        ensure!(
            answer["error"]["data"]["code"] == json!(baseline),
            "`{tool}` is refused differently from an unknown tool ({baseline}): {answer}"
        );
    }
    Ok(())
}
