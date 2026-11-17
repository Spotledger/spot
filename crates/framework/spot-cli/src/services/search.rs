//! Search of one site: the search projection (`spot-pg-search`) installed in the site's storage,
//! its start-up check, its rebuild job, and the port that serves the `search` clause of the
//! resource query (contract 0.22.0, WP-STO-13.T6, ADR-087 section 7).
//!
//! `spot-http` owns the request shape (`api::router::routes::search`) and
//! `spot-api-services::search` owns the query (permission filter, facets, snippets, cursors).
//! This module hosts them on a running site:
//!
//! - [`install_hook`]: builds the `SearchHook` for the site's metadata (protected fields grouped
//!   so they never enter the projection) and installs it on the backend **before `prepare`**, so
//!   a DocType with a `[search]` declaration gets its projection at prepare and on every write in
//!   the same transaction. The backend is built before the services register, so the host calls
//!   this one function where it builds the backend (one line, see the task report);
//! - the **start-up check** (`Service::check`): if a DocType declares search and the database
//!   cannot serve it (the `pg_trgm` extension is missing, or the database ctype cannot build Urdu
//!   trigrams) the service is *degraded* with the exact fix, and search answers
//!   `501 search.unsupported`, never silently weaker results;
//! - the **rebuild job** `platform:search.rebuild` with `{"doctype": ...}`: recreates one
//!   DocType's projection in one transaction; searches keep answering from the old projection
//!   until the transaction commits;
//! - the port: [`SearchService`](spot_api_services::search::SearchService) behind the field guard
//!   (channel `Search`), the permission-filtering engine and the backend's capability and lag.
//!
//! # Level
//!
//! Optional. A degraded or failed start leaves the site serving documents; searches answer
//! `501 search.unsupported`. Settings are `[services.search]`: `enabled`, `language`,
//! `max_rebuild_rows_per_second`, `snippet_max_chars`, `lag_bound_ms`. No search term is ever
//! logged or put in a metric label.

#[path = "search/adapters.rs"]
pub mod adapters;

use std::sync::Arc;

use serde::Deserialize;
use spot_api_services::engine_resources::ActorResolver;
use spot_api_services::search::{EngineSearchStore, SearchService, SearchStore};
use spot_app_api::context::{Actor, Invocation};
use spot_http::api::ports::search::{
    SearchExtensionIn, SearchPageOut, SearchPort, SearchResult, SearchableOut,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, QuerySpec, RequestCtx};
use spot_pg_search::{Language, Protection, SearchHook};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Name, StorageBackend as _};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::permissions::GuardedEngine;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

use self::adapters::{GuardAccess, PgProjection, SearchState};

/// Start order: after the services that provide what the port needs.
pub const ORDER: u32 = 1070;

/// The job the rebuild runs under.
pub const REBUILD_JOB: &str = "platform:search.rebuild";

/// `[services.search]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves search at all.
    pub enabled: bool,
    /// The tenant's default text language (a DocType's declaration wins).
    pub language: String,
    /// The most projection rows a rebuild writes per second, on average.
    pub max_rebuild_rows_per_second: u64,
    /// The longest snippet a request may ask for, in characters (the contract's maximum is 200).
    pub snippet_max_chars: u32,
    /// The projection's lag, in milliseconds, past which a search is `503 search.projection_behind`.
    pub lag_bound_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
            language: "en".to_owned(),
            max_rebuild_rows_per_second: 5_000,
            snippet_max_chars: 200,
            lag_bound_ms: 5_000,
        }
    }
}

impl Settings {
    /// `Err(key)` for a value out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if Language::new(&self.language).is_err() {
            return Err("language");
        }
        if !(1..=1_000_000).contains(&self.max_rebuild_rows_per_second) {
            return Err("max_rebuild_rows_per_second");
        }
        if !(20..=200).contains(&self.snippet_max_chars) {
            return Err("snippet_max_chars");
        }
        if !(1..=3_600_000).contains(&self.lag_bound_ms) {
            return Err("lag_bound_ms");
        }
        Ok(())
    }
}

/// Installs the search hook on `backend` for the DocTypes of `meta`: the projection is created at
/// `prepare` and maintained on every write, and search is served. Fields in a protection group
/// are grouped so they never enter the projection (ADR-087 7.7).
///
/// The host calls this where it builds the site's backend, before `prepare`.
///
/// # Errors
///
/// A language tag that is not usable, or a protection group name the projection refuses.
pub fn install_hook(
    backend: PgBackend,
    meta: &spot_meta::EffectiveMeta,
    language: &str,
) -> Result<PgBackend> {
    use spot_field_guard::{MetaProtection, ProtectionSource as _};
    let language =
        Language::new(language).map_err(|e| Error(format!("services.search.language: {e}")))?;
    let source = MetaProtection::new(meta);
    let mut protection = Protection::new();
    for def in meta.doctypes() {
        let name = def.name.as_str().to_owned();
        let Some(facts) = source.protection(&name) else {
            continue;
        };
        let entity = Name::new(&name).map_err(|e| Error(format!("search: {e}")))?;
        for field in &facts.fields {
            let column = Name::new(&field.path).map_err(|e| Error(format!("search: {e}")))?;
            for group in &field.groups {
                protection = protection
                    .group_field(&entity, &column, group.as_str())
                    .map_err(|e| Error(format!("search: {e}")))?;
            }
        }
    }
    Ok(SearchHook::install(backend, language, protection))
}

/// What the start-up check says to an operator when the ctype cannot build Urdu trigrams.
fn ctype_fix(detail: &str) -> String {
    format!(
        "search cannot serve Urdu or typo tolerance: {detail}. Recreate the tenant database with a \
         Unicode ctype (`LOCALE_PROVIDER libc, LC_CTYPE 'C.UTF-8'`; the platform's `create_database` \
         does this) and restore the data, or turn search off with `[services.search] enabled = false`"
    )
}

struct SearchHostService {
    state: SearchState,
    live: Arc<SiteLive>,
    backend: PgBackend,
}

impl SearchHostService {
    /// Whether any installed DocType declares search.
    fn declares_search(&self) -> bool {
        let Some(served) = self.live.served() else {
            return false;
        };
        crate::schema::catalog(&served.meta)
            .map(|catalog| catalog.entities().any(|e| e.search.is_some()))
            .unwrap_or(false)
    }
}

impl Service for SearchHostService {
    fn name(&self) -> &'static str {
        "search"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if !self.declares_search() {
                self.state.set(None);
                return Ok(());
            }
            if !self.backend.capabilities().search {
                let text = "a DocType declares search but the search hook is not installed in the \
                            site's storage (the host must call services::search::install_hook \
                            before prepare)"
                    .to_owned();
                self.state.set(Some(text.clone()));
                return Err(text);
            }
            // pg_trgm present and an Urdu word has trigrams: the one check both rest on. A missing
            // `unaccent` is refused by `prepare` itself, with the extension named.
            match spot_postgres::check_text_search_ctype(self.backend.config()).await {
                Ok(()) => {
                    self.state.set(None);
                    Ok(())
                }
                Err(error) => {
                    let text = ctype_fix(&error.explanation);
                    self.state.set(Some(text.clone()));
                    Err(text)
                }
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

tokio::task_local! {
    /// The caller `SearchGate` authenticated for the call running now.
    static CALLER: Actor;
}

fn caller_actor() -> std::result::Result<Actor, ApiError> {
    CALLER
        .try_with(Clone::clone)
        .map_err(|_| ApiError::from_code(Code::AuthRequired))
}

/// The store over the site's live handles: builds the permission-filtering engine over the
/// metadata served now for every call, so a metadata swap shows on the next search.
struct LiveStore {
    live: Arc<SiteLive>,
}

impl LiveStore {
    fn inner(&self) -> std::result::Result<EngineSearchStore<GuardedEngine>, ApiError> {
        let served = self
            .live
            .served()
            .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))?;
        let engine = self
            .live
            .engine()
            .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))?;
        let authorizer = self
            .live
            .authorizer()
            .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))?;
        let guarded = Arc::new(GuardedEngine::new(engine, authorizer, Arc::default()));
        Ok(EngineSearchStore::new(guarded, Arc::clone(&served.meta)))
    }
}

impl SearchStore for LiveStore {
    fn catalog(&self) -> std::result::Result<Arc<Catalog>, ApiError> {
        self.inner()?.catalog()
    }

    fn doctype_meta<'a>(
        &'a self,
        doctype: &'a str,
    ) -> BoxFuture<'a, std::result::Result<spot_http::api::meta::DocTypeMeta, ApiError>> {
        Box::pin(async move { self.inner()?.doctype_meta(doctype).await })
    }

    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a spot_storage_contract::Query,
    ) -> BoxFuture<'a, std::result::Result<spot_storage_contract::Rows, ApiError>> {
        Box::pin(async move { self.inner()?.read(inv, catalog, query).await })
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: spot_storage_contract::RecordId,
    ) -> BoxFuture<'a, std::result::Result<spot_http::api::codec::ApiDocument, ApiError>> {
        Box::pin(async move { self.inner()?.load(inv, doctype, id).await })
    }
}

/// The port: authenticates the caller, then runs the search as that caller.
struct SearchGate {
    site: String,
    live: Arc<SiteLive>,
    service: SearchService,
}

impl SearchGate {
    async fn actor(&self, ctx: &RequestCtx) -> std::result::Result<Actor, ApiError> {
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        let auth = self
            .live
            .auth()
            .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        Ok(Actor::User {
            id: who.user_id,
            via: None,
        })
    }
}

impl SearchPort for SearchGate {
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
        search: &'a SearchExtensionIn,
    ) -> BoxFuture<'a, SearchResult<SearchPageOut>> {
        Box::pin(async move {
            let actor = self.actor(ctx).await?;
            CALLER
                .scope(actor, self.service.query(ctx, doctype, spec, search))
                .await
        })
    }

    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchableOut>> {
        Box::pin(async move {
            let actor = self.actor(ctx).await?;
            CALLER
                .scope(actor, self.service.describe(ctx, doctype))
                .await
        })
    }
}

/// Runs one rebuild of `doctype`'s projection in one transaction. Searches keep answering from
/// the old projection until it commits; a failure rolls back and leaves the old projection intact.
async fn rebuild(
    live: &SiteLive,
    backend: &PgBackend,
    language: &str,
    doctype: &str,
) -> std::result::Result<u64, String> {
    use spot_storage_contract::{Isolation, StorageBackend as _, Transaction as _, TxOptions};
    let served = live.served().ok_or("no metadata is served")?;
    let catalog = Arc::new(crate::schema::catalog(&served.meta).map_err(|e| e.to_string())?);
    let language = Language::new(language).map_err(|e| e.to_string())?;
    let name = Name::new(doctype).map_err(|e| e.to_string())?;
    let plans = spot_pg_search::planned_doctypes(&catalog, &language, &Protection::new())
        .map_err(|e| e.to_string())?;
    let plan = plans
        .into_iter()
        .find(|p| p.doctype() == &name)
        .ok_or("the DocType declares no search")?;
    let options = TxOptions::new(
        Name::new("search_rebuild").map_err(|e| e.to_string())?,
        Isolation::ReadCommitted,
    );
    let mut tx = backend
        .begin(catalog, options)
        .await
        .map_err(|e| e.to_string())?;
    match spot_pg_search::rebuild(&mut tx, &plan).await {
        Ok(report) => {
            tx.commit().await.map_err(|e| e.to_string())?;
            Ok(report.projected)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error.to_string())
        }
    }
}

/// Registers search: the start-up check service, the rebuild job and the port.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("search")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.search.{key}: out of range")))?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.as_str().to_owned();
    let live = Arc::clone(env.live);
    let state = SearchState::default();
    let backend = env.backend.clone();
    if ctx.assembling() {
        ctx.add(Arc::new(SearchHostService {
            state: state.clone(),
            live: Arc::clone(&live),
            backend: backend.clone(),
        }));
        let (live_job, backend_job, language) = (
            Arc::clone(&live),
            backend.clone(),
            settings.language.clone(),
        );
        ctx.job_handlers().register(REBUILD_JOB, move |job| {
            let (live, backend, language) =
                (Arc::clone(&live_job), backend_job.clone(), language.clone());
            Box::pin(async move {
                let doctype = job
                    .job
                    .args
                    .get("doctype")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_owned();
                Ok(match rebuild(&live, &backend, &language, &doctype).await {
                    // The DocType is never in the message: only the outcome.
                    Ok(_) => spot_jobs::worker::HandlerResult::Ok,
                    Err(reason) => spot_jobs::worker::HandlerResult::Retry(reason),
                })
            })
        });
    }
    let actor: Arc<ActorResolver> = Arc::new(|_| caller_actor());
    let service = SearchService::new(
        Arc::new(LiveStore {
            live: Arc::clone(&live),
        }),
        Arc::new(GuardAccess {
            live: Arc::clone(&live),
            clock: Arc::clone(ctx.clock()),
        }),
        Arc::clone(ctx.clock()),
        actor,
    )
    .with_projection(Arc::new(PgProjection {
        backend,
        state,
        bound_ms: settings.lag_bound_ms,
    }));
    ctx.provide::<dyn SearchPort>(Arc::new(SearchGate {
        site,
        live,
        service,
    }))?;
    Ok(())
}
