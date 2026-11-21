//! Retrieval over the running server (WP-AI-10.T3): the acceptance sentence of WP-AI-10, "a
//! restricted user's agent never surfaces an unreadable document", end to end over `POST /mcp`.
//!
//! These cases are **blocked**, and the harness shows the distance rather than hiding it. The
//! extension-tool seam the `retrieve` tool installs through is served (WP-AI-04.T6 and T7: the
//! retrieval service puts the tool on `tools/list` while it is ready). One piece is still outside
//! the product:
//!
//! - no service provides the embedding port yet (the LLM service's `EmbedPort`, WP-AI-03.T7), so
//!   a running site has no embedder, the retrieval service reports itself degraded
//!   (`retrieval.embedder_missing`), nothing is indexed and `retrieve` is not on `tools/list`
//!   (the absent-tool half is the real case `agent_tools: the_agent_tools_are_absent_...`).
//!
//! The behaviour itself is proved without the network in `crates/framework/spot-cli/tests/
//! retrieval_service.rs` (the real coordinator lease, the platform document reader, the real field
//! guard, the in-memory and the PostgreSQL stores), which is the suite that fails if the live
//! permission re-check is ever skipped. When the embedding port is served these cases become runs:
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
            "WP-AI-03.T7",
            "the harness site configures no LLM tenant with an `embedding_model` (the LLM service provides the embedding port only then, WP-AI-03.T7), so retrieval is degraded on it, nothing is indexed and `retrieve` is not on tools/list; the tool and its extension-tool adapter are proved in-process by `retrieval_service.rs`",
        ),
        CaseDef::blocked(
            "retrieval: a_revoked_share_applies_to_the_next_retrieve_call",
            SECTION,
            "after the share that made a document readable is revoked over REST, the very next `retrieve` call returns no hit for it, with no re-index in between",
            "WP-AI-03.T7",
            "the harness site configures no LLM tenant with an `embedding_model` (WP-AI-03.T7), so it has no embedding port and cannot index; the in-process proof is `retrieval_service.rs`",
        ),
    ]
}
