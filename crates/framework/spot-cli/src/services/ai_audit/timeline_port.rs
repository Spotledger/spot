//! The host adapter of the AI timeline route (WP-AI-11.T11): `GET
//! /api/v1/resource/{doctype}/{id}/ai-timeline` (`resource_ai_timeline`, contract 0.28.0) served
//! over the hosted [`AiAuditPort`].
//!
//! `spot-http` owns the route and the [`AiTimelinePort`] seam; this module supplies the rest and
//! adds **no access logic of its own**: what the caller may see is [`AiAuditPort::timeline`]'s
//! answer, decided by the engine-backed [`ReadAccess`](super::ReadAccess) of WP-AI-11.T9 as the
//! caller.
//!
//! # Who the viewer is
//!
//! The caller, and only the caller: the bearer in the request is authenticated with the site's
//! sign-in service on every call and the viewer text is made from the authenticated user, never
//! from the path, the query or a body (the port is given no viewer id and nothing in the route
//! names one). An API key authenticates as its owner, so the owner's record id is the viewer. An
//! agent principal (a user whose type is `agent`) keeps the spelling `agent:<id>`, so the access
//! judges it by its own roles. A credential of another audience than staff or partner, one still
//! restricted to choosing a password and a request for another site are `403 permission.denied`:
//! the route table refuses a consumer on the registry's routers, and this module refuses it again
//! because the group is mounted by this service, not by the registry pass.
//!
//! # Answers
//!
//! - the entries the access allows, newest first, as [`TimelineRow`]s: the instant is the chain's
//!   Unix microseconds as RFC 3339 UTC (a pure conversion; no clock is read), an argument value
//!   longer than the contract's 128 characters is shown as `***` and at most 32 arguments are
//!   kept (the chain never writes more);
//! - a valid empty answer (no right, a missing document, an id or DocType that names nothing) is
//!   an empty list: the audit service already answers that way;
//! - **any** failure of the audit service ([`TIMELINE_REFUSED`]: the access could not be decided;
//!   [`CHAIN_UNAVAILABLE`]: the chain could not be read) or an instant that cannot be written is
//!   `503 server.unavailable` with `Retry-After` and never entries, never an empty list that would
//!   read as "no access".
//!
//! A fixed refusal text is used for every error; only the stable `ai_audit_host.*` code is logged,
//! never a document id, a user, an argument name or a value.
//!
//! The adapter is provided as `dyn AiTimelinePort` by the `ai_audit` module when the module is
//! enabled (so it has the chain), and the module mounts the route group itself with the port:
//! the host's registry pass knows the group as service-mounted. With the module off, nothing is
//! provided or mounted and the operation is unserved.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_core::Audience;
use spot_http::api::Ports;
use spot_http::api::ports::ai_timeline::{AiTimelinePort, TimelineRow};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::api::storage_error::RETRY_AFTER_DETAIL;
use spot_iam_directory::UserType;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{AiAuditPort, TimelineEntry};
use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::SiteCtx;
use crate::{Error, Result};

/// The contract's most arguments in one entry.
const MAX_ARGUMENTS: usize = 32;
/// The contract's longest argument value, in characters.
const MAX_VALUE_CHARS: usize = 128;
/// The marker of a value that may not be shown (the same literal the access's masking uses).
const HIDDEN: &str = "***";
/// Seconds a client is told to wait after a `503`.
const RETRY_AFTER_SECS: u64 = 5;

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable)
        .detail(RETRY_AFTER_DETAIL, Json::from(RETRY_AFTER_SECS))
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionDenied)
}

/// Who is asking, as the audit service's viewer text. The seam exists so the adapter is tested
/// without a sign-in service; [`LiveCallers`] is the production implementation.
pub trait Callers: Send + Sync {
    /// The viewer text of the caller of `ctx`: `<user id>`, or `agent:<id>` for an agent user.
    ///
    /// # Errors
    ///
    /// `401` for a credential that is not valid, `403 permission.denied` for an audience that is
    /// not staff or partner, a restricted credential or another site, `503` when the sign-in
    /// service is not available.
    fn viewer<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<String, ApiError>>;
}

/// The production [`Callers`]: the site's sign-in service, read from the live hub on every call
/// (a reload swaps the service and the next call uses the new one).
pub struct LiveCallers {
    site: String,
    live: Arc<SiteLive>,
}

impl LiveCallers {
    /// Callers of `site` over `live`.
    #[must_use]
    pub fn new(site: String, live: Arc<SiteLive>) -> Self {
        Self { site, live }
    }

    async fn resolve(&self, ctx: &RequestCtx) -> std::result::Result<String, ApiError> {
        if ctx.site.as_str() != self.site {
            return Err(denied());
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if !matches!(who.audience, Audience::Staff | Audience::Partner) || auth.is_restricted(&who)
        {
            return Err(denied());
        }
        Ok(viewer_text(&who.user_id.to_string(), who.user_type))
    }
}

impl Callers for LiveCallers {
    fn viewer<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<String, ApiError>> {
        Box::pin(self.resolve(ctx))
    }
}

/// The viewer text of an authenticated user: an agent keeps its `agent:` spelling, everyone else
/// is the bare record id.
#[must_use]
pub fn viewer_text(user_id: &str, user_type: UserType) -> String {
    match user_type {
        UserType::Agent => format!("agent:{user_id}"),
        _ => user_id.to_owned(),
    }
}

/// The contract's instant for Unix microseconds: RFC 3339 in UTC, ending in `Z`. `None` for a value
/// the format cannot write (outside years 0 to 9999). A pure conversion: no clock is read.
#[must_use]
pub fn instant_text(micros: i64) -> Option<String> {
    let nanos = i128::from(micros).checked_mul(1_000)?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
    at.format(&Rfc3339).ok()
}

/// One entry as a row, or `None` when its instant cannot be written.
fn row_of(entry: TimelineEntry) -> Option<TimelineRow> {
    let at = instant_text(entry.at_micros)?;
    let arguments = entry
        .arguments
        .into_iter()
        .take(MAX_ARGUMENTS)
        .map(|(name, value)| {
            let shown = if value.chars().count() > MAX_VALUE_CHARS {
                HIDDEN.to_owned()
            } else {
                value
            };
            (name, shown)
        })
        .collect();
    Some(TimelineRow {
        seq: entry.seq,
        at,
        kind: entry.kind,
        agent: entry.agent,
        acting_user: entry.acting_user,
        tool: entry.tool,
        outcome: entry.outcome,
        arguments,
    })
}

/// The [`AiTimelinePort`] over the hosted AI audit service.
pub struct TimelineAdapter<C> {
    audit: Arc<dyn AiAuditPort>,
    callers: C,
}

impl<C: Callers> TimelineAdapter<C> {
    /// An adapter over `audit` that learns who is asking from `callers`.
    #[must_use]
    pub fn new(audit: Arc<dyn AiAuditPort>, callers: C) -> Self {
        Self { audit, callers }
    }

    async fn run(
        &self,
        ctx: &RequestCtx,
        doctype: &str,
        id: &str,
        limit: usize,
    ) -> std::result::Result<Vec<TimelineRow>, ApiError> {
        let viewer = self.callers.viewer(ctx).await?;
        let entries = match self.audit.timeline(&viewer, doctype, id, limit).await {
            Ok(entries) => entries,
            Err(error) => {
                // The stable code only: never the viewer, the document or a value.
                tracing::warn!(code = error.code, "the AI timeline could not be answered");
                return Err(unavailable());
            }
        };
        let mut rows = Vec::with_capacity(entries.len().min(limit));
        for entry in entries.into_iter().take(limit) {
            match row_of(entry) {
                Some(row) => rows.push(row),
                None => {
                    tracing::warn!(
                        code = super::CHAIN_UNAVAILABLE,
                        "an AI timeline entry has an instant that cannot be written"
                    );
                    return Err(unavailable());
                }
            }
        }
        Ok(rows)
    }
}

impl<C: Callers> AiTimelinePort for TimelineAdapter<C> {
    fn timeline<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, std::result::Result<Vec<TimelineRow>, ApiError>> {
        Box::pin(self.run(ctx, doctype, id, limit))
    }
}

/// Provides the timeline port for the site's hosted `audit` (so a later module may read it) and
/// mounts the route group over it (the host's registry pass classifies the group as service
/// mounted).
///
/// # Errors
///
/// The port is already provided; the group cannot be mounted.
pub(super) fn provide(ctx: &mut SiteCtx<'_>, audit: Arc<dyn AiAuditPort>) -> Result<()> {
    let env = *ctx.env()?;
    let callers = LiveCallers::new(env.site.site.to_string(), Arc::clone(env.live));
    let port: Arc<dyn AiTimelinePort> = Arc::new(TimelineAdapter::new(audit, callers));
    ctx.provide::<dyn AiTimelinePort>(Arc::clone(&port))?;
    let mut ports = Ports::new();
    ports.provide::<dyn AiTimelinePort>(port);
    let mounted = routes::mount(&[routes::ai_timeline::registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        return Err(Error(format!(
            "the AI timeline route was not mounted: {module}: {reason:?}"
        )));
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::super::{
        AiAction, AiAuditError, CHAIN_UNAVAILABLE, Fut, Receipt, ReviewDigest, TIMELINE_REFUSED,
    };
    use super::*;

    /// A caller that is always `viewer`, or refused.
    struct Fixed(std::result::Result<String, ApiError>);

    impl Callers for Fixed {
        fn viewer<'a>(
            &'a self,
            _ctx: &'a RequestCtx,
        ) -> BoxFuture<'a, std::result::Result<String, ApiError>> {
            let answer = self.0.clone();
            Box::pin(async move { answer })
        }
    }

    /// An audit service that answers a script and remembers what it was asked.
    struct Scripted {
        answer: std::result::Result<Vec<TimelineEntry>, &'static str>,
        asked: Mutex<Vec<(String, String, String, usize)>>,
    }

    impl Scripted {
        fn new(answer: std::result::Result<Vec<TimelineEntry>, &'static str>) -> Arc<Self> {
            Arc::new(Self {
                answer,
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl AiAuditPort for Scripted {
        fn append(&self, _action: AiAction) -> Fut<'_, std::result::Result<Receipt, AiAuditError>> {
            Box::pin(async { Ok(Receipt::Duplicate) })
        }

        fn timeline<'a>(
            &'a self,
            viewer: &'a str,
            doctype: &'a str,
            id: &'a str,
            limit: usize,
        ) -> Fut<'a, std::result::Result<Vec<TimelineEntry>, AiAuditError>> {
            self.asked.lock().unwrap().push((
                viewer.to_owned(),
                doctype.to_owned(),
                id.to_owned(),
                limit,
            ));
            let answer = self.answer.clone().map_err(|code| AiAuditError { code });
            Box::pin(async move { answer })
        }

        fn latest_digest(&self) -> Option<ReviewDigest> {
            None
        }
    }

    fn entry(seq: u64, at_micros: i64) -> TimelineEntry {
        TimelineEntry {
            seq,
            at_micros,
            kind: "tool_call".to_owned(),
            agent: "bot1".to_owned(),
            acting_user: "alice".to_owned(),
            tool: "update_task".to_owned(),
            outcome: "ok".to_owned(),
            arguments: vec![("title".to_owned(), "plan".to_owned())],
        }
    }

    fn ctx() -> RequestCtx {
        RequestCtx {
            site: spot_core::SiteId::new("acme").unwrap(),
            credential: Some(spot_http::api::service::Credential::new("t")),
            idempotency_key: None,
            language: None,
            request_id: spot_http::request::RequestId::generate(),
            peer: None,
        }
    }

    fn adapter(
        audit: &Arc<Scripted>,
        viewer: std::result::Result<String, ApiError>,
    ) -> TimelineAdapter<Fixed> {
        TimelineAdapter::new(audit.clone(), Fixed(viewer))
    }

    #[test]
    fn an_agent_keeps_its_spelling_and_everyone_else_is_the_bare_id() {
        assert_eq!(viewer_text("u1", UserType::Agent), "agent:u1");
        for kind in [UserType::Internal, UserType::Portal, UserType::Service] {
            assert_eq!(viewer_text("u1", kind), "u1");
        }
    }

    #[test]
    fn an_instant_is_utc_rfc3339_ending_in_z_and_within_the_contracts_bound() {
        assert_eq!(
            instant_text(1_700_000_000_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(
            instant_text(1_700_000_000_123_456).as_deref(),
            Some("2023-11-14T22:13:20.123456Z")
        );
        assert_eq!(instant_text(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert!(
            instant_text(i64::MAX).is_none(),
            "year 292277 cannot be written"
        );
        for micros in [0, 1_700_000_000_123_456, -1_000_000] {
            let text = instant_text(micros).unwrap();
            assert!(text.ends_with('Z') && text.len() <= 40, "{text}");
        }
    }

    #[tokio::test]
    async fn the_viewer_is_the_caller_and_the_entries_become_rows() {
        let audit = Scripted::new(Ok(vec![
            entry(9, 1_700_000_000_000_000),
            entry(4, 1_600_000_000_000_000),
        ]));
        let port = adapter(&audit, Ok("user-1".to_owned()));
        let rows = port.timeline(&ctx(), "task", "T-1", 25).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 9);
        assert_eq!(rows[0].at, "2023-11-14T22:13:20Z");
        assert_eq!(
            rows[0].arguments,
            vec![("title".to_owned(), "plan".to_owned())]
        );
        assert_eq!(rows[1].seq, 4);
        let asked = audit.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![("user-1".to_owned(), "task".to_owned(), "T-1".to_owned(), 25)]
        );
    }

    #[tokio::test]
    async fn an_empty_answer_is_an_empty_list() {
        let audit = Scripted::new(Ok(Vec::new()));
        let port = adapter(&audit, Ok("user-1".to_owned()));
        assert!(
            port.timeline(&ctx(), "task", "T-1", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_failure_of_the_audit_service_is_the_503_with_retry_after_and_never_entries() {
        for code in [TIMELINE_REFUSED, CHAIN_UNAVAILABLE] {
            let audit = Scripted::new(Err(code));
            let port = adapter(&audit, Ok("user-1".to_owned()));
            let error = port.timeline(&ctx(), "task", "T-1", 10).await.unwrap_err();
            assert_eq!(
                error.code.as_str(),
                Code::ServerUnavailable.as_str(),
                "{code}"
            );
            assert_eq!(
                spot_http::api::storage_error::retry_after_secs(&error),
                Some(RETRY_AFTER_SECS),
                "{code}"
            );
            // The code of the failure is not shown to the caller.
            assert!(
                !error.message.contains("ai_audit_host"),
                "{}",
                error.message
            );
        }
    }

    #[tokio::test]
    async fn a_caller_that_is_refused_never_reaches_the_audit_service() {
        let audit = Scripted::new(Ok(vec![entry(1, 0)]));
        for refusal in [
            denied(),
            ApiError::from_code(Code::AuthSessionExpired),
            unavailable(),
        ] {
            let expected = refusal.code.as_str().to_owned();
            let port = adapter(&audit, Err(refusal));
            let error = port.timeline(&ctx(), "task", "T-1", 10).await.unwrap_err();
            assert_eq!(error.code.as_str(), expected);
        }
        assert!(audit.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_entry_that_cannot_be_written_is_the_503_not_a_clipped_answer() {
        let audit = Scripted::new(Ok(vec![entry(1, i64::MAX)]));
        let port = adapter(&audit, Ok("user-1".to_owned()));
        let error = port.timeline(&ctx(), "task", "T-1", 10).await.unwrap_err();
        assert_eq!(error.code.as_str(), Code::ServerUnavailable.as_str());
    }

    #[tokio::test]
    async fn a_value_over_the_contracts_bound_is_hidden_and_the_pairs_are_capped() {
        let mut long = entry(1, 0);
        long.arguments = (0..40)
            .map(|i| (format!("a{i}"), "v".repeat(if i == 0 { 129 } else { 128 })))
            .collect();
        let audit = Scripted::new(Ok(vec![long]));
        let port = adapter(&audit, Ok("user-1".to_owned()));
        let rows = port.timeline(&ctx(), "task", "T-1", 10).await.unwrap();
        assert_eq!(rows[0].arguments.len(), MAX_ARGUMENTS);
        assert_eq!(rows[0].arguments[0].1, HIDDEN);
        assert_eq!(rows[0].arguments[1].1.len(), 128);
    }

    #[tokio::test]
    async fn never_more_rows_than_the_limit() {
        let audit = Scripted::new(Ok(vec![entry(3, 0), entry(2, 0), entry(1, 0)]));
        let port = adapter(&audit, Ok("user-1".to_owned()));
        assert_eq!(
            port.timeline(&ctx(), "task", "T-1", 2).await.unwrap().len(),
            2
        );
    }
}
