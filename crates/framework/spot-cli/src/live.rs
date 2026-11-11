//! What a site serves *right now*, and the adapters that read it once per request.
//!
//! Runtime customisation (WP-PLT-16) changes a site's metadata while the server runs. The
//! pieces that capture metadata at construction (`EngineResources`, the permission view, the
//! `/meta` service) are therefore not built once: [`LiveResources`], [`LivePermissions`] and
//! [`SiteLive::meta_source`] read the current [`Served`] pair on every request, so the request
//! after a swap sees the new DocTypes with no restart and no rebuild of the router.
//!
//! [`SiteLive`] is the one hub per site. It outlives a `spot reload` (a reload rebuilds the
//! router and the engine from the files, then *starts from what is served now*) and holds:
//!
//! - the served metadata and the permission decision core built for it;
//! - the engine and authorizer of the running runtime, which a swap points at the new metadata;
//! - the default grants the customised DocTypes carry (a new DocType is closed to everyone
//!   except `System Manager` until someone grants more);
//! - the first problem a swap hit, which readiness reports while the old metadata keeps serving.
//!
//! A swap never holds a lock across an await: the cells are read, cloned and released.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use serde_json::Value as Json;
use spot_api_services::{
    AccessControl, CurrentMeta, EngineResources, MetaSource, PermissionResources, PrincipalSource,
};
use spot_app_api::context::Actor;
use spot_customise::MetaGate;
use spot_document::engine::{Engine, PermissionAuthorizer};
use spot_http::api::codec::{ApiDocument, DocumentPatchApi};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, BoxFuture, Message, MethodOutcome, Page, PermissionService, QuerySpec, RequestCtx,
    ResourceService, WriteResult,
};
use spot_meta::EffectiveMeta;
use spot_permissions::{Model, PermissionEngine, Right, Rights, RoleGrant};
use spot_postgres::PgBackend;
use spot_storage_contract::RecordId;

use crate::login::SiteAuth;
use crate::permissions::{self, AppPerms, GuardedEngine, StoragePrincipals};

/// The metadata a site serves and the permission decision core built for it. The two always
/// change together.
pub struct Served {
    /// The effective metadata.
    pub meta: Arc<EffectiveMeta>,
    /// The decision core over that metadata and the site's grants.
    pub decision: Arc<PermissionEngine>,
    /// The snapshot version it was swapped in at; `None` for the metadata a runtime started
    /// with.
    pub version: Option<u64>,
}

/// The engine and authorizer of the running runtime.
#[derive(Clone)]
struct Target {
    engine: Arc<Engine<PgBackend>>,
    authorizer: Arc<PermissionAuthorizer>,
    principals: Arc<StoragePrincipals>,
}

/// The customise operations, as the resource adapter sees them: one call per admin method.
pub trait Admin: Send + Sync {
    /// Runs the admin method `function` for `user` with `args`.
    ///
    /// # Errors
    ///
    /// The API error to answer with: `permission.denied` for a caller who may not customise,
    /// the validation problems all at once for a change that does not validate.
    fn call<'a>(
        &'a self,
        user: RecordId,
        function: &'a str,
        args: Json,
    ) -> BoxFuture<'a, Result<Json, ApiError>>;
}

/// The hub of one site's live state.
pub struct SiteLive {
    served: RwLock<Option<Arc<Served>>>,
    target: Mutex<Option<Target>>,
    model: Mutex<Model>,
    apps: Mutex<AppPerms>,
    grants: Mutex<Vec<RoleGrant>>,
    problem: Mutex<Option<String>>,
    degraded: Mutex<Vec<String>>,
    customised: AtomicBool,
    gate: OnceLock<Arc<MetaGate>>,
    admin: OnceLock<Arc<dyn Admin>>,
    auth: Mutex<Option<Arc<SiteAuth>>>,
    swaps: tokio::sync::watch::Sender<u64>,
    /// The pair a rebuild attached but the router does not serve yet; see [`SiteLive::commit`].
    pending: Mutex<Option<(Arc<EffectiveMeta>, Arc<PermissionEngine>)>>,
    realtime: OnceLock<Arc<crate::services::realtime::Realtime>>,
    files: OnceLock<Arc<crate::services::files::Files>>,
    print: OnceLock<Arc<crate::services::print::Printing>>,
    mail: OnceLock<Arc<crate::services::mail::Mail>>,
    /// How the transaction participants are rebuilt for a new metadata generation
    /// (WP-PRD-01.T11), set by the first runtime build.
    participants: Mutex<Option<Arc<ParticipantsFactory>>>,
}

/// Rebuilds one site's transaction participants for one metadata generation, in the order the
/// engine requires, or says why they cannot be installed.
///
/// The participants are per generation: edge sync derives its schema from the metadata and the
/// audit masker masks with it, so a metadata swap has to install the new generation's
/// participants on the same engine. `spot_cli::host::build_site` records this, and
/// [`SiteLive::swap_to`] runs it.
pub type ParticipantsFactory = dyn Fn(
        &Arc<EffectiveMeta>,
    ) -> std::result::Result<
        Vec<Arc<dyn spot_document::engine::TxParticipant<spot_postgres::PgTransaction>>>,
        String,
    > + Send
    + Sync;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Default for SiteLive {
    fn default() -> Self {
        Self::new()
    }
}

impl SiteLive {
    /// An empty hub: nothing is served until the first runtime is attached.
    pub fn new() -> Self {
        Self {
            served: RwLock::new(None),
            target: Mutex::new(None),
            model: Mutex::new(Model::default()),
            apps: Mutex::new(AppPerms::default()),
            grants: Mutex::new(Vec::new()),
            problem: Mutex::new(None),
            degraded: Mutex::new(Vec::new()),
            customised: AtomicBool::new(false),
            gate: OnceLock::new(),
            admin: OnceLock::new(),
            auth: Mutex::new(None),
            swaps: tokio::sync::watch::channel(0).0,
            pending: Mutex::new(None),
            realtime: OnceLock::new(),
            files: OnceLock::new(),
            print: OnceLock::new(),
            mail: OnceLock::new(),
            participants: Mutex::new(None),
        }
    }

    /// The pair served now, if a runtime was attached.
    pub fn served(&self) -> Option<Arc<Served>> {
        self.served
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The metadata a rebuild of the site starts from: what is served now once a customisation
    /// has been swapped in, and `None` before that (the files alone are then the truth, which
    /// keeps the file-recorded runtime changes of `metadata.json` working).
    pub fn rebuild_from(&self) -> Option<Arc<EffectiveMeta>> {
        if self.customised.load(Ordering::SeqCst) {
            self.served().map(|served| Arc::clone(&served.meta))
        } else {
            None
        }
    }

    /// The default grants of the customised DocTypes, to merge into a rebuilt model.
    pub fn grants(&self) -> Vec<RoleGrant> {
        lock(&self.grants).clone()
    }

    /// Makes `engine` and `authorizer` the runtime that swaps are applied to, and `meta` with
    /// `decision` what is served. Called by every runtime build (the first, and a reload).
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        &self,
        meta: Arc<EffectiveMeta>,
        model: Model,
        apps: AppPerms,
        decision: Arc<PermissionEngine>,
        engine: Arc<Engine<PgBackend>>,
        authorizer: Arc<PermissionAuthorizer>,
        principals: Arc<StoragePrincipals>,
    ) {
        *lock(&self.model) = model;
        *lock(&self.apps) = apps;
        *lock(&self.target) = Some(Target {
            engine,
            authorizer,
            principals,
        });
        // The very first runtime serves at once. A rebuild's pair waits for [`commit`]: until
        // the router runs the new runtime, the requests still on the old one must see the old
        // metadata, or they would read a field the old engine and its permission snapshot do
        // not know yet (a 403 on a field that /meta already lists).
        if self.served().is_none() {
            *self.served.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(Served {
                meta,
                decision,
                version: None,
            }));
            self.swaps.send_modify(|count| *count += 1);
        } else {
            *lock(&self.pending) = Some((meta, decision));
        }
    }

    /// Serves the pair of the last [`attach`](Self::attach), once the host has put the runtime
    /// built with it in front of requests. A no-op when a swap already moved past it or the
    /// attach served at once.
    pub fn commit(&self) {
        let Some((meta, decision)) = lock(&self.pending).take() else {
            return;
        };
        // A rebuild keeps the snapshot version the site was serving.
        let version = self.served().and_then(|served| served.version);
        *self.served.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(Served {
            meta,
            decision,
            version,
        }));
        self.swaps.send_modify(|count| *count += 1);
    }

    /// The engine of the running runtime: documents read or written as the platform itself.
    pub fn engine(&self) -> Option<Arc<Engine<PgBackend>>> {
        lock(&self.target).as_ref().map(|t| Arc::clone(&t.engine))
    }

    /// The authorizer of the running runtime: the decision for one user on one document.
    pub fn authorizer(&self) -> Option<Arc<PermissionAuthorizer>> {
        lock(&self.target)
            .as_ref()
            .map(|t| Arc::clone(&t.authorizer))
    }

    /// The file service of this site, created by `make` the first time and kept for the life of
    /// the process (the memory provider holds its bytes in it).
    pub fn files_with(
        &self,
        make: impl FnOnce() -> Arc<crate::services::files::Files>,
    ) -> Arc<crate::services::files::Files> {
        Arc::clone(self.files.get_or_init(make))
    }

    /// The file service, once created.
    pub fn files(&self) -> Option<Arc<crate::services::files::Files>> {
        self.files.get().cloned()
    }

    /// The mail service of this site, created by `make` the first time.
    pub fn mail_with(
        &self,
        make: impl FnOnce() -> Arc<crate::services::mail::Mail>,
    ) -> Arc<crate::services::mail::Mail> {
        Arc::clone(self.mail.get_or_init(make))
    }

    /// The mail service, once created.
    pub fn mail(&self) -> Option<Arc<crate::services::mail::Mail>> {
        self.mail.get().cloned()
    }

    /// The print service of this site, created by `make` the first time and kept for the life
    /// of the process (it holds the parsed fonts).
    pub fn print_with(
        &self,
        make: impl FnOnce() -> Arc<crate::services::print::Printing>,
    ) -> Arc<crate::services::print::Printing> {
        Arc::clone(self.print.get_or_init(make))
    }

    /// The print service, once created.
    pub fn print(&self) -> Option<Arc<crate::services::print::Printing>> {
        self.print.get().cloned()
    }

    /// The storage-backed source of users' assignments of the running runtime.
    pub fn principals(&self) -> Option<Arc<StoragePrincipals>> {
        lock(&self.target)
            .as_ref()
            .map(|t| Arc::clone(&t.principals))
    }

    /// Makes `auth` the sign-in service the realtime sockets verify tokens with (every runtime
    /// build sets it, so a reload that changed the site's keys reaches new connections).
    pub fn set_auth(&self, auth: Arc<SiteAuth>) {
        *lock(&self.auth) = Some(auth);
    }

    /// The sign-in service of the running runtime.
    pub fn auth(&self) -> Option<Arc<SiteAuth>> {
        lock(&self.auth).clone()
    }

    /// A receiver that changes every time the permission snapshot is replaced (a runtime
    /// build or a customisation swap).
    pub fn swaps(&self) -> tokio::sync::watch::Receiver<u64> {
        self.swaps.subscribe()
    }

    /// The realtime hub of this site, created by `make` the first time and kept for the life
    /// of the process, so a reload keeps the bus and its connections.
    pub fn realtime_with(
        &self,
        make: impl FnOnce() -> Arc<crate::services::realtime::Realtime>,
    ) -> Arc<crate::services::realtime::Realtime> {
        Arc::clone(self.realtime.get_or_init(make))
    }

    /// The realtime hub, once created.
    pub fn realtime(&self) -> Option<Arc<crate::services::realtime::Realtime>> {
        self.realtime.get().cloned()
    }

    /// Records the snapshot version the site serves, when none is known yet, so the first
    /// customisation's version is a different one.
    pub fn stamp_version(&self, version: u64) {
        let mut served = self.served.write().unwrap_or_else(|p| p.into_inner());
        if let Some(current) = served.as_ref()
            && current.version.is_none()
        {
            *served = Some(Arc::new(Served {
                meta: Arc::clone(&current.meta),
                decision: Arc::clone(&current.decision),
                version: Some(version),
            }));
        }
    }

    /// Registers the gate whose divergence stops this site serving metadata (once).
    pub fn set_gate(&self, gate: Arc<MetaGate>) {
        let _ = self.gate.set(gate);
    }

    /// Registers the customise operations (once).
    pub fn set_admin(&self, admin: Arc<dyn Admin>) {
        let _ = self.admin.set(admin);
    }

    /// Records or clears the problem readiness reports.
    pub fn set_problem(&self, problem: Option<String>) {
        *lock(&self.problem) = problem;
    }

    /// Records what the site serves with a reduced function, for readiness to say: it stays
    /// in rotation. Replaces the previous list, so a build that fixed the cause clears it.
    pub fn set_degraded(&self, degraded: Vec<String>) {
        *lock(&self.degraded) = degraded;
    }

    /// What the site serves degraded now.
    pub fn degraded(&self) -> Vec<String> {
        lock(&self.degraded).clone()
    }

    /// Why the site cannot serve its metadata right now: a swap that failed, or a divergence
    /// from the metadata its peers announced.
    pub fn problem(&self) -> Option<String> {
        if let Some(gate) = self.gate.get()
            && let Some(report) = gate.report()
        {
            return Some(format!("metadata diverged: {report}"));
        }
        lock(&self.problem).clone()
    }

    /// The metadata to answer this request with, or the divergence that forbids it.
    ///
    /// # Errors
    ///
    /// `server.unavailable` while the node is diverged or no runtime was attached.
    pub fn current(&self) -> Result<Arc<Served>, ApiError> {
        if let Some(gate) = self.gate.get()
            && let Some(report) = gate.report()
        {
            return Err(ApiError::new(
                Code::ServerUnavailable,
                "This node cannot serve metadata that differs from its peers' yet.",
            )
            .detail("reason", Json::String(report)));
        }
        self.served()
            .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))
    }

    /// The `/meta` service's source: the served metadata, read per request.
    pub fn meta_source(self: &Arc<Self>) -> Arc<dyn MetaSource> {
        let live = Arc::clone(self);
        Arc::new(move || {
            live.current().map(|served| CurrentMeta {
                meta: Arc::clone(&served.meta),
                version: served.version,
            })
        })
    }

    /// Records how a metadata swap rebuilds the participants. Set by every runtime build; the
    /// last one wins, which is the newest generation's view of the site.
    pub fn set_participants_factory(&self, factory: Arc<ParticipantsFactory>) {
        *lock(&self.participants) = Some(factory);
    }

    /// Points the engine, the permission snapshot and the served pair at `meta`.
    ///
    /// `defaults` are the default grants of the customised DocTypes. Order matters: the engine
    /// first (it compiles the catalogue and prepares storage, so it can refuse), then the
    /// permission snapshot, and only then the served pair, so a failure at any step leaves the
    /// previous metadata fully in force.
    ///
    /// # Errors
    ///
    /// A sentence naming the step that failed.
    pub async fn swap_to(
        &self,
        meta: Arc<EffectiveMeta>,
        version: u64,
        defaults: Vec<RoleGrant>,
    ) -> Result<(), String> {
        let target = lock(&self.target)
            .clone()
            .ok_or_else(|| "no runtime is attached to swap".to_owned())?;
        let mut model = lock(&self.model).clone();
        model.grants.extend(defaults.iter().cloned());
        let apps = lock(&self.apps).clone();
        let decision = permissions::engine(&meta, &model, &apps).map_err(|e| e.to_string())?;
        // The participants of the new generation, built before the engine takes the metadata: a
        // refusal here leaves the previous metadata and its participants fully in force. They are
        // installed after the metadata, so no write sees the new metadata with the old schema.
        let participants = match &*lock(&self.participants) {
            Some(factory) => Some(factory(&meta)?),
            None => None,
        };
        target
            .engine
            .set_meta(Arc::clone(&meta))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(participants) = participants {
            target.engine.set_participants(participants);
        }
        target.authorizer.swap(Arc::clone(&decision));
        *lock(&self.pending) = None;
        *lock(&self.grants) = defaults;
        self.customised.store(true, Ordering::SeqCst);
        *self.served.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(Served {
            meta,
            decision,
            version: Some(version),
        }));
        self.swaps.send_modify(|count| *count += 1);
        Ok(())
    }

    /// The `System Manager` default grant of one customised DocType.
    pub fn default_grant(doctype: &str, role: &str) -> RoleGrant {
        RoleGrant {
            doctype: doctype.to_owned(),
            role: spot_permissions::Role(role.to_owned()),
            rights: Rights::new([Right::Read, Right::Write, Right::Create, Right::Delete]),
            if_owner: false,
        }
    }
}

/// The permission summary, built over the metadata and decision core served now.
pub struct LivePermissions {
    pub(crate) live: Arc<SiteLive>,
    pub(crate) principals: Arc<dyn PrincipalSource>,
    pub(crate) clock: Arc<dyn spot_core::Clock>,
    pub(crate) actor: Arc<spot_api_services::engine_resources::ActorResolver>,
}

impl PermissionService for LivePermissions {
    fn summary<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctypes: &'a [spot_storage_contract::Name],
    ) -> BoxFuture<'a, Result<spot_http::api::permissions::PermissionSummary, ApiError>> {
        Box::pin(async move {
            let served = self.live.current()?;
            let access = AccessControl::new(
                Arc::clone(&served.meta),
                Arc::clone(&served.decision),
                Arc::clone(&self.principals),
                0,
                Arc::clone(&self.clock),
                Arc::clone(&self.actor),
            );
            PermissionResources::new(Arc::new(access))
                .summary(ctx, doctypes)
                .await
        })
    }
}

/// The document operations, with an `EngineResources` built over the metadata served now for
/// every call, and the customise operations as admin methods.
pub struct LiveResources {
    pub(crate) live: Arc<SiteLive>,
    pub(crate) engine: Arc<GuardedEngine>,
    pub(crate) clock: Arc<dyn spot_core::Clock>,
    pub(crate) actor: Arc<spot_api_services::engine_resources::ActorResolver>,
    /// The function and report delegates a hosting module provided as ports (WP-PRD-01.T48).
    pub(crate) delegates: spot_api_services::Delegates,
}

impl LiveResources {
    fn current(&self) -> Result<EngineResources<GuardedEngine>, ApiError> {
        let served = self.live.current()?;
        Ok(EngineResources::new(
            Arc::clone(&self.engine),
            Arc::clone(&served.meta),
            Arc::clone(&self.clock),
            Arc::clone(&self.actor),
        )
        .with_delegates(self.delegates.clone()))
    }
}

/// Hosting modules a test process adds beside the built-in and generated ones.
static EXTRA_MODULES: Mutex<Vec<crate::services::Registration>> = Mutex::new(Vec::new());

/// Adds hosting modules to every site this process builds from now on, after the built-in and the
/// generated ones, with the same duplicate-name and start-order rules.
///
/// This is the seam a test uses to host a probe module through the public
/// [`Registration`](crate::services::Registration) type (WP-PRD-01.T48); `spot serve` never calls
/// it, so the production table is only what `build.rs` generates. Not part of the stable surface.
#[doc(hidden)]
pub fn add_extra_modules(modules: &[crate::services::Registration]) {
    lock(&EXTRA_MODULES).extend_from_slice(modules);
}

/// The modules [`add_extra_modules`] added.
pub(crate) fn extra_modules() -> Vec<crate::services::Registration> {
    lock(&EXTRA_MODULES).clone()
}

/// The app and module the admin methods are served under.
pub const ADMIN_APP: &str = "platform";
/// The module of the customise admin methods.
pub const ADMIN_MODULE: &str = "customise";

impl ResourceService for LiveResources {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current()?.get(ctx, doctype, id).await })
    }

    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move { self.current()?.list(ctx, doctype, spec).await })
    }

    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move { self.current()?.query(ctx, doctype, spec).await })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current()?.create(ctx, doctype, document).await })
    }

    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a DocumentPatchApi,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            self.current()?
                .update(ctx, doctype, id, patch, expected)
                .await
        })
    }

    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<Vec<Message>, ApiError>> {
        Box::pin(async move { self.current()?.delete(ctx, doctype, id, expected).await })
    }

    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current()?.post(ctx, doctype, id, expected).await })
    }

    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current()?.cancel(ctx, doctype, id, expected).await })
    }

    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current()?.correct(ctx, doctype, id, expected).await })
    }

    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            self.current()?
                .rename(ctx, doctype, id, new_name, expected)
                .await
        })
    }

    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<spot_app_api::prelude::Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<Message>), ApiError>> {
        Box::pin(async move {
            self.current()?
                .call_method(ctx, doctype, id, method, args, expected)
                .await
        })
    }

    fn call_function<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        Box::pin(async move {
            if app == ADMIN_APP
                && module == ADMIN_MODULE
                && let Some(admin) = self.live.admin.get()
            {
                // Only `list` reads; every other admin method changes the site and is a POST.
                if via_get && function != "list" {
                    return Err(ApiError::new(
                        Code::ApiInvalidRequest,
                        format!("`{function}` changes the site; call it with POST."),
                    ));
                }
                let actor = match (self.actor)(ctx)? {
                    Actor::User { id, .. } => id,
                    _ => return Err(ApiError::from_code(Code::AuthRequired)),
                };
                let data = admin.call(actor, function, args).await?;
                return Ok((data, Vec::new()));
            }
            self.current()?
                .call_function(ctx, app, module, function, args, via_get)
                .await
        })
    }

    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(spot_http::api::service::ReportResult, Vec<Message>), ApiError>>
    {
        Box::pin(async move {
            self.current()?
                .run_report(ctx, report, filters, limit)
                .await
        })
    }
}

/// The readiness check of one site's live metadata: a swap that failed, or a divergence from
/// the metadata the other nodes announced, takes the site out of rotation (`503`) while the
/// previous metadata keeps serving whatever it can.
pub struct LiveReady {
    name: String,
    live: Arc<SiteLive>,
}

impl LiveReady {
    /// The check for `live`, reported under `metadata:<site>`.
    pub fn new(site: &str, live: Arc<SiteLive>) -> Self {
        Self {
            name: format!("metadata:{site}"),
            live,
        }
    }
}

impl spot_http::health::ReadinessCheck for LiveReady {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = spot_http::health::CheckResult> + Send + '_>,
    > {
        Box::pin(async move {
            match (self.live.problem(), self.live.degraded()) {
                (Some(problem), _) => spot_http::health::CheckResult::failed(problem),
                (None, degraded) if degraded.is_empty() => spot_http::health::CheckResult::ok(),
                (None, degraded) => spot_http::health::CheckResult {
                    ok: true,
                    detail: format!("degraded: {}", degraded.join("; ")),
                },
            }
        })
    }
}
