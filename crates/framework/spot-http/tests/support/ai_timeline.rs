//! A timeline port that answers one fixed entry, for the suites that need the
//! `resource_ai_timeline` group mounted (the conformance walk). The route's own behaviour is
//! tested with a scripted fake in `tests/ai_timeline_routes.rs`; who may see what belongs to the
//! host's adapter and is tested in `spot-cli`.

use std::sync::Arc;

use spot_http::api::ApiError;
use spot_http::api::ports::ai_timeline::{AiTimelinePort, TimelineRow};
use spot_http::api::service::{BoxFuture, RequestCtx};

/// Answers one entry, whatever the document.
pub struct OneEntry;

impl AiTimelinePort for OneEntry {
    fn timeline<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _doctype: &'a str,
        _id: &'a str,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TimelineRow>, ApiError>> {
        Box::pin(async {
            Ok(vec![TimelineRow {
                seq: 3,
                at: "2026-11-21T09:05:00.000001Z".to_owned(),
                kind: "tool_call".to_owned(),
                agent: "agent-1".to_owned(),
                acting_user: "user-1".to_owned(),
                tool: "update_doc".to_owned(),
                outcome: "ok".to_owned(),
                arguments: vec![("title".to_owned(), "***".to_owned())],
            }])
        })
    }
}

/// The port as a suite provides it.
pub fn fake() -> Arc<dyn AiTimelinePort> {
    Arc::new(OneEntry)
}
