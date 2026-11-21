//! The AI timeline seam of `GET /api/v1/resource/{doctype}/{id}/ai-timeline`
//! (`resource_ai_timeline`, WP-AI-11.T11, contract 0.28.0, ADR-066): the AI actions that touched
//! one document, as the caller may see them.
//!
//! `spot-http` holds no audit chain and decides no access. It defines [`AiTimelinePort`], one
//! read in the shape the router needs, and the host writes the adapter over the AI audit service.
//! Everything that decides is the port's: who the caller is (from `ctx.credential` on `ctx.site`,
//! never from the path, a query value or a body), whether the caller may read the document, which
//! of the entry's arguments the caller may see (a masked viewer gets the literal `***`), and
//! whether the access could be decided at all (an undecidable access is the port's `503
//! server.unavailable`, never an empty list that would read as "no access"). The route passes no
//! viewer id: there is no way to ask for another user's view through this trait.
//!
//! [`TimelineRow`] mirrors the contract's `AiTimelineEntry`, with plain types: no `spot-cli` type
//! reaches this crate. A row never carries a prompt, a model output, a tool result or a document
//! field value, and the route builds its answer from the rows and nothing else (no count, no hint
//! of hidden entries).

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`AiTimelinePort`].
pub const KEY: PortKey = PortKey::of::<dyn AiTimelinePort>();

/// One AI action on a document, as the caller may see it (the contract's `AiTimelineEntry`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineRow {
    /// The entry's sequence number on the AI audit chain.
    pub seq: u64,
    /// When it happened: RFC 3339 in UTC (a trailing `Z`), at most 40 characters.
    pub at: String,
    /// `tool_call`, `approval`, `rejection` or `undo` (an open set: a client shows an unknown
    /// kind as text).
    pub kind: String,
    /// The agent's id.
    pub agent: String,
    /// The user the agent acted for, or the reviewing user.
    pub acting_user: String,
    /// The tool's name.
    pub tool: String,
    /// `ok`, `refused` or `failed`.
    pub outcome: String,
    /// The tool's arguments by name, at most 32; a value the caller may not see is `***`; a value
    /// is at most 128 characters.
    pub arguments: Vec<(String, String)>,
}

/// The AI entries of one document for the caller.
pub trait AiTimelinePort: Send + Sync {
    /// The AI entries of `doctype`/`id` the caller (`ctx.credential`) may see, newest first, at
    /// most `limit` (the route passes 1 to 100). An empty list is the answer for a caller with no
    /// read right, a missing document and an id or DocType that names nothing.
    ///
    /// # Errors
    ///
    /// A registry [`ApiError`], passed through by the route with its code, status and details:
    /// `503 server.unavailable` (with its `retry_after` detail, which the route turns into
    /// `Retry-After`) when the caller's access cannot be decided or the chain cannot be read, `401`
    /// for a credential that is no longer valid, `403 permission.denied` for an audience that is
    /// not staff or partner.
    fn timeline<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TimelineRow>, ApiError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_names_the_port_type() {
        assert_eq!(KEY, PortKey::of::<dyn AiTimelinePort>());
    }
}
