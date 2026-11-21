//! The access questions on a running server (WP-IAM-08.T9): `spot-iam-questions` over the live
//! directory, provided as the port [`AccessQuestions`] for the routes that serve them.
//!
//! - **Model**: the live model of [`access_questions_model`](super::access_questions_model)
//!   (WP-IAM-08.T13): the stored roles and assignments, the installed apps' grants, duties and
//!   SoD rules, the portal users and the permission-change history, read from one read-only
//!   transaction and cached per role-snapshot version. A role, assignment or user change reaches it
//!   through the outbox (WP-IAM-08.T15, [`ChangeFeedService`]): the question after an assignment
//!   changes answers differently without a restart, and a missed event is repaired by the periodic
//!   full read. If the model cannot be built the answer is the model's stable code
//!   (`access_questions_model.*`): never an answer from a partial model.
//! - **Who may ask**: the crate decides (`access_governance` duty, or a question about oneself); this
//!   module passes the authenticated asker only, never a name from the request, and every question,
//!   refused ones included, writes a `QueryAudit` line to `spot::audit::access_questions` (asker,
//!   question, counts; never rows).
//! - **Bounds**: `Page` limits are the crate's (`MAX_PAGE`); the range of `access_changes` is capped by
//!   `max_range_days`.
//!
//! Settings `[services.access_questions]`: `enabled` (true), `max_range_days` (366, 1 to 3660). Level:
//! optional. Not here (see the report): emergency access, documents (so `who_can_see`).

use std::sync::Arc;

use serde::Deserialize;
use spot_core::Clock;
use spot_http::api::service::BoxFuture;
use spot_iam_questions::{
    Change, Holder, Page, Paged, PermissionRow, PermissionSpec, QueryAudit, QueryAuditor,
    Questions, QuestionsError, ScopeQuery,
};
use time::OffsetDateTime;

use crate::Result;
use crate::services::access_questions_model::feed::ChangeFeedService;
use crate::services::access_questions_model::live::{LiveModelSource, ModelSettings, from_site};
use crate::services::ports::OutboxLease;
use crate::services::{Idle, Level, SiteCtx};

/// After the governance gate (1070).
pub const ORDER: u32 = 1080;

const AUDIT: &str = "spot::audit::access_questions";

/// Why a question has no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskError {
    /// The model cannot be built now: refused, fail closed.
    Unavailable,
    /// The crate refused (asker not allowed, malformed question): its stable code.
    Refused(&'static str),
}

impl AskError {
    /// The stable code a caller maps to its own answer.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => "access_questions_host.unavailable",
            Self::Refused(code) => code,
        }
    }
}

/// The questions, asked as the authenticated `asker` (a user id from the credential).
pub trait AccessQuestions: Send + Sync {
    /// What `user` can do, and why.
    fn what_can_user_do<'a>(
        &'a self,
        asker: &'a str,
        user: &'a str,
        page: Page,
    ) -> BoxFuture<'a, Result2<Paged<PermissionRow>>>;
    /// Who can do `spec` (in `scope`).
    fn who_can_do<'a>(
        &'a self,
        asker: &'a str,
        spec: &'a PermissionSpec,
        scope: &'a ScopeQuery,
        page: Page,
    ) -> BoxFuture<'a, Result2<Paged<Holder>>>;
    /// What changed between the two instants (unix seconds, at most `max_range_days` apart).
    fn access_changes<'a>(
        &'a self,
        asker: &'a str,
        from: i64,
        to: i64,
        page: Page,
    ) -> BoxFuture<'a, Result2<Paged<Change>>>;
}

/// The answer type of the port.
pub type Result2<T> = std::result::Result<T, AskError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    max_range_days: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_range_days: 366,
        }
    }
}

struct Auditor {
    site: String,
}

impl QueryAuditor for Auditor {
    fn record(&self, audit: &QueryAudit) {
        tracing::info!(
            target: AUDIT,
            site = %self.site,
            asker = %audit.asker,
            question = %audit.question,
            allowed = audit.allowed,
            total = audit.total,
            returned = audit.returned,
            "access question"
        );
    }
}

/// The access questions of one site.
pub struct Service {
    site: String,
    source: Arc<LiveModelSource>,
    clock: Arc<dyn Clock>,
    max_range_days: i64,
}

impl Service {
    /// The questions over `source`.
    #[must_use]
    pub fn new(
        site: String,
        source: Arc<LiveModelSource>,
        clock: Arc<dyn Clock>,
        max_range_days: i64,
    ) -> Self {
        Self {
            site,
            source,
            clock,
            max_range_days,
        }
    }

    fn now(&self) -> OffsetDateTime {
        self.clock.now_utc()
    }

    /// The model for one question; the model's stable code when it cannot be built.
    async fn model(
        &self,
        window: Option<(i64, i64)>,
    ) -> Result2<crate::services::access_questions_model::live::LiveAccessModel> {
        self.source.model(window).await.map_err(|error| {
            tracing::warn!(code = error.code(), "the access model cannot answer");
            AskError::Refused(error.code())
        })
    }
}

fn refused(error: &QuestionsError) -> AskError {
    AskError::Refused(error.code())
}

impl AccessQuestions for Service {
    fn what_can_user_do<'a>(
        &'a self,
        asker: &'a str,
        user: &'a str,
        page: Page,
    ) -> BoxFuture<'a, Result2<Paged<PermissionRow>>> {
        Box::pin(async move {
            let model = self.model(None).await?;
            let auditor = Auditor {
                site: self.site.clone(),
            };
            Questions::new(&model, &auditor)
                .what_can_user_do(asker, user, self.now(), page)
                .map_err(|e| refused(&e))
        })
    }

    fn who_can_do<'a>(
        &'a self,
        asker: &'a str,
        spec: &'a PermissionSpec,
        scope: &'a ScopeQuery,
        page: Page,
    ) -> BoxFuture<'a, Result2<Paged<Holder>>> {
        Box::pin(async move {
            let model = self.model(None).await?;
            let auditor = Auditor {
                site: self.site.clone(),
            };
            Questions::new(&model, &auditor)
                .who_can_do(asker, spec, scope, self.now(), page)
                .map_err(|e| refused(&e))
        })
    }

    fn access_changes<'a>(
        &'a self,
        asker: &'a str,
        from: i64,
        to: i64,
        page: Page,
    ) -> BoxFuture<'a, Result2<Paged<Change>>> {
        Box::pin(async move {
            if to < from || to - from > self.max_range_days.saturating_mul(86_400) {
                return Err(AskError::Refused("iam_questions.request_invalid"));
            }
            let model = self.model(Some((from, to))).await?;
            let auditor = Auditor {
                site: self.site.clone(),
            };
            Questions::new(&model, &auditor)
                .access_changes(asker, from, to, self.now(), page)
                .map_err(|e| refused(&e))
        })
    }
}

/// Joins the site unless `[services.access_questions] enabled = false`.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("access_questions")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(1..=3660).contains(&settings.max_range_days) {
        return Err(crate::Error(
            "services.access_questions.max_range_days: out of range".to_owned(),
        ));
    }
    let site = ctx.env()?.site.site.clone();
    let source = Arc::new(from_site(ctx, ModelSettings::default())?);
    if ctx.assembling() {
        let health = Arc::clone(&source);
        ctx.add(Idle::new("access_questions", Level::Optional, move || {
            let health = Arc::clone(&health);
            Box::pin(async move { health.status().await.readiness() })
        }));
        // Role, assignment and user changes reach the model through the outbox, when the site has
        // one; without it the model re-reads on its timer.
        if let Some(outbox) = ctx.port::<dyn OutboxLease>() {
            ctx.add(Arc::new(ChangeFeedService::new(
                outbox,
                Arc::clone(&source),
            )));
        }
    }
    let service: Arc<dyn AccessQuestions> = Arc::new(Service::new(
        site,
        source,
        Arc::clone(ctx.clock()),
        settings.max_range_days,
    ));
    ctx.provide(service)?;
    Ok(())
}
