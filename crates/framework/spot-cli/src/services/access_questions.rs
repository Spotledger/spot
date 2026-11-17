//! The access questions on a running server (WP-IAM-08.T9): `spot-iam-questions` over the live
//! directory, provided as the port [`AccessQuestions`] for the routes that serve them.
//!
//! - **Model**: the roles stored on the site (`role` documents with their duty names), the duty
//!   definitions the installed apps ship (`<app>/duties.toml`: `[[duty]]` tables of the crate's
//!   `Duty`), the active assignments (`scim_store::load`), the portal users (`outside`), the
//!   effective metadata of the serving snapshot and the SoD rules (`<app>/sod_rules.toml`). It is
//!   rebuilt for every question (no cache yet) from one read-only transaction, so an answer reflects
//!   one point in time. If the roles and duties cannot build a registry the answer is
//!   `access_questions_host.unavailable`: never an answer from a partial model.
//! - **Who may ask**: the crate decides (`access_governance` duty, or a question about oneself); this
//!   module passes the authenticated asker only, never a name from the request, and every question,
//!   refused ones included, writes a `QueryAudit` line to `spot::audit::access_questions` (asker,
//!   question, counts; never rows).
//! - **Bounds**: `Page` limits are the crate's (`MAX_PAGE`); the range of `access_changes` is capped by
//!   `max_range_days`.
//!
//! Settings `[services.access_questions]`: `enabled` (true), `max_range_days` (366, 1 to 3660). Level:
//! optional. Not here (see the report): emergency access, the permission-change history (the model's
//! `history` is empty until the audit stream is readable), documents and shares (so `who_can_see`).

use std::sync::Arc;

use serde::Deserialize;
use spot_core::Clock;
use spot_http::api::service::BoxFuture;
use spot_iam_governance::audit::ChangeEvent;
use spot_iam_questions::{
    AccessModel, Change, Holder, Page, Paged, PermissionRow, PermissionSpec, QueryAudit,
    QueryAuditor, Questions, QuestionsError, ScopeQuery,
};
use spot_iam_roles::{Assignment, Duty, Registry};
use spot_meta::EffectiveMeta;
use spot_sod::RuleSet;
use spot_storage_contract::{Catalog, Isolation, StorageBackend, Transaction, TxOptions};
use time::OffsetDateTime;

use crate::Result;
use crate::live::SiteLive;
use crate::services::scim_store::{load, nm, rows, text};
use crate::services::{Idle, Level, SiteCtx};

/// After the governance gate (1070).
pub const ORDER: u32 = 1080;

const AUDIT: &str = "spot::audit::access_questions";
const DUTIES_FILE: &str = "duties.toml";
const RULES_FILE: &str = "sod_rules.toml";

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

#[derive(Deserialize)]
struct DutyFile {
    #[serde(default)]
    duty: Vec<Duty>,
}

/// The model of one point in time.
struct Live {
    registry: Registry,
    assignments: Vec<Assignment>,
    portal: Vec<String>,
    rules: RuleSet,
    history: Vec<ChangeEvent>,
    meta: Arc<EffectiveMeta>,
}

impl AccessModel for Live {
    fn registry(&self) -> &Registry {
        &self.registry
    }
    fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }
    fn portal_users(&self) -> &[String] {
        &self.portal
    }
    fn rules(&self) -> &RuleSet {
        &self.rules
    }
    fn history(&self) -> &[ChangeEvent] {
        &self.history
    }
    fn meta(&self) -> &EffectiveMeta {
        &self.meta
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
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
    duties: Vec<Duty>,
    rules: RuleSet,
    max_range_days: i64,
}

impl Service {
    async fn model(&self) -> Result2<Live> {
        let engine = self.live.engine().ok_or(AskError::Unavailable)?;
        let served = self.live.current().map_err(|_| AskError::Unavailable)?;
        let options = TxOptions::new(
            nm("access_questions").map_err(|_| AskError::Unavailable)?,
            Isolation::ReadCommitted,
        )
        .read_only();
        let mut tx = engine
            .backend()
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| AskError::Unavailable)?;
        let unit = load(&self.site, &mut tx).await;
        let stored = rows(&mut tx, spot_platform_app::doctype::ROLE, &["role_name"]).await;
        let portal = rows(
            &mut tx,
            spot_platform_app::doctype::USER,
            &["id", "user_type"],
        )
        .await;
        let _ = tx.rollback().await;
        let (unit, stored, portal) = (
            unit.map_err(|_| AskError::Unavailable)?,
            stored.map_err(|_| AskError::Unavailable)?,
            portal.map_err(|_| AskError::Unavailable)?,
        );
        let roles = stored
            .iter()
            .filter_map(|r| text(&r[0]))
            .map(|name| spot_iam_roles::Role::custom(&name))
            .collect();
        let registry =
            Registry::new(self.duties.clone(), roles).map_err(|_| AskError::Unavailable)?;
        let portal = portal
            .iter()
            .filter(|r| text(&r[1]).as_deref() == Some("portal"))
            .filter_map(|r| match &r[0] {
                spot_storage_contract::Value::Id(id) => Some(id.to_string()),
                _ => None,
            })
            .collect();
        Ok(Live {
            registry,
            assignments: unit.assignments,
            portal,
            rules: self.rules.clone(),
            history: Vec::new(),
            meta: Arc::clone(&served.meta),
        })
    }

    fn now(&self) -> OffsetDateTime {
        self.clock.now_utc()
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
            let model = self.model().await?;
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
            let model = self.model().await?;
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
            let model = self.model().await?;
            let auditor = Auditor {
                site: self.site.clone(),
            };
            Questions::new(&model, &auditor)
                .access_changes(asker, from, to, self.now(), page)
                .map_err(|e| refused(&e))
        })
    }
}

fn app_text(ctx: &SiteCtx<'_>, name: &str) -> Result<String> {
    let env = *ctx.env()?;
    let mut text = String::new();
    for app in super::controllers::installed(env.root, env.loaded, env.site)? {
        if let Ok(part) = std::fs::read_to_string(app.dir.join(name)) {
            text.push_str(&part);
            text.push('\n');
        }
    }
    Ok(text)
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
    let (site, live, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let mut problem: Option<String> = None;
    let duties = match toml::from_str::<DutyFile>(&app_text(ctx, DUTIES_FILE)?) {
        Ok(file) => file.duty,
        Err(_) => {
            problem = Some(format!("{DUTIES_FILE} does not parse"));
            Vec::new()
        }
    };
    let sod = app_text(ctx, RULES_FILE)?;
    let rules = if sod.trim().is_empty() {
        RuleSet::default()
    } else {
        match RuleSet::from_toml(&sod) {
            Ok(rules) => rules,
            Err(_) => {
                problem = Some(format!("{RULES_FILE} does not parse"));
                RuleSet::default()
            }
        }
    };
    if ctx.assembling() {
        let report = problem.clone().or_else(|| {
            duties.is_empty().then(|| {
                format!("no app ships {DUTIES_FILE}: the access questions cannot build a registry")
            })
        });
        ctx.add(Idle::new("access_questions", Level::Optional, move || {
            let report = report.clone();
            Box::pin(async move { report.map_or(Ok(()), Err) })
        }));
    }
    let service: Arc<dyn AccessQuestions> = Arc::new(Service {
        site,
        live,
        clock: Arc::clone(ctx.clock()),
        catalog,
        duties,
        rules,
        max_range_days: settings.max_range_days,
    });
    ctx.provide(service)?;
    Ok(())
}
