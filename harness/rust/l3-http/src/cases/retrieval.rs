//! Retrieval over the running server (WP-AI-10.T3): the acceptance sentence of WP-AI-10, "a
//! restricted user's agent never surfaces an unreadable document", end to end over `POST /mcp`.
//!
//! These cases are **blocked**, and the harness shows the distance rather than hiding it. Two
//! pieces the end-to-end run needs are outside this task's paths:
//!
//! - the MCP service has no hook to install an extra tool, so the `retrieve` tool the retrieval
//!   service provides (`spot_cli::services::retrieval::tool::RetrieveTool`, provided as a port) is
//!   not on `tools/list`: `spot-mcp`'s `ToolKind` is a closed enum and `McpHandle` has no tool slot;
//! - no service provides the embedding port yet (the LLM service's `EmbedPort` provider), so a
//!   running site has no embedder and the retrieval service reports itself degraded.
//!
//! The behaviour itself is proved without the network in `crates/framework/spot-cli/tests/
//! retrieval_service.rs` (the real coordinator lease, the platform document reader, the real field
//! guard, the in-memory and the PostgreSQL stores), which is the suite that fails if the live
//! permission re-check is ever skipped. When both blocks above are lifted these cases become runs:
//! index a seeded corpus on a site with `[services.retrieval]` on, call `tools/call retrieve` as two
//! users, revoke a share over REST and call again at once.

use super::CaseDef;

/// After the MCP cases (70); each case would own its fixture.
pub const ORDER: u32 = 75;

const SECTION: &str = "Retrieval";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "retrieval: a_restricted_users_agent_never_surfaces_an_unreadable_document",
            SECTION,
            "an agent signed in as a restricted user calls the `retrieve` MCP tool and gets no hit for a document that user may not read, and the unreadable document's text is in no snippet",
            "WP-AI-10.T3",
            "the MCP service has no extra-tool hook (`spot-mcp` ToolKind is closed and `McpHandle` has no tool slot), so `retrieve` is not on tools/list over HTTP; the tool exists as a port and is tested in-process",
        ),
        CaseDef::blocked(
            "retrieval: a_revoked_share_applies_to_the_next_retrieve_call",
            SECTION,
            "after the share that made a document readable is revoked over REST, the very next `retrieve` call returns no hit for it, with no re-index in between",
            "WP-AI-03.T5",
            "no service provides the embedding port yet, so a running site cannot index; the in-process proof is `retrieval_service.rs`",
        ),
    ]
}
