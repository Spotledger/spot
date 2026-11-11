//! Runtime customisation in `spot serve`: a DocType, a custom field or a property override made
//! while the site runs, served at once on this node and followed by the others (WP-PLT-16).
//!
//! [`Customise`] builds `spot-customise`'s [`CustomiseService`] over the site's own parts:
//! the live metadata and its version store and bus ([`LiveMetadata`]), the customisation
//! records in the platform DocTypes over the site's PostgreSQL, and the online migration engine
//! behind the schema step. It adds the host's half of the contract:
//!
//! - **swap**: after every swap the engine is pointed at the new metadata, the permission
//!   snapshot is rebuilt for its DocTypes (each customised DocType keeps the one default grant:
//!   `System Manager` read, write, create, delete) and the metadata served per request is
//!   replaced, in that order. A failure leaves the previous metadata serving and is reported
//!   on `/api/health/ready`; the next poll retries it;
//! - **start-up**: [`CustomiseService::recover`] runs in this service's check, before the site
//!   accepts traffic, so a crash between two pipeline steps converges;
//! - **following**: a node learns of another node's change by polling the version store and
//!   rebuilding from the shared records (which are the truth, the version is only a claim about
//!   them); a node that cannot reproduce what was announced refuses to serve metadata and says
//!   so on readiness (`metalive.divergence`);
//! - **access**: [`may_customise`] is the one place that decides who customises: the caller
//!   holds `Tenant Administrator` or `System Manager`. No new `Right` exists for it (the
//!   task's open question), so replacing this check with one is a one-line change here;
//! - **operations**: `create_doctype`, `update_doctype`, `add_custom_field`,
//!   `remove_custom_field`, `set_property`, `clear_property` and `list`, served as admin
//!   methods under `/api/v1/method/platform/customise/<operation>`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_api_services::PrincipalSource;
use spot_customise::{
    AuditEntry, AuditSink, Caller, CustomiseAccess, CustomiseError, CustomiseEvent,
    CustomiseService, DefaultGrant, EventSink, GrantSink, ListenerFuture, Parts, RecordState,
    RecordStore, ReferrerLookup, SYSTEM_MANAGER, SchemaBackend, SchemaFuture, SchemaPlan,
    StorageRecordStore, SwapListener,
};
use spot_customise::{Fill, Referrer};
use spot_meta::{BuildOptions, FieldSpec};
use spot_meta_live::Snapshot;
use spot_platform_app::role;
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, RecordId};
use tokio::sync::watch;

use crate::Result;
use crate::live::{Admin, SiteLive};
use crate::services::metadata::LiveMetadata;
use crate::services::{CheckFuture, Level, RunFuture, Service};

/// Whether `roles` may customise the site: a `Tenant Administrator` or a `System Manager`.
///
/// The only place the question is decided (the brief's recommendation: a role check behind
/// [`CustomiseAccess`], not a new `Right`).
#[must_use]
pub fn may_customise(roles: &BTreeSet<String>) -> bool {
    roles.contains(role::TENANT_ADMINISTRATOR) || roles.contains(role::SYSTEM_MANAGER)
}

struct RoleCheck;

impl CustomiseAccess for RoleCheck {
    fn may_customise(&self, caller: &Caller) -> bool {
        may_customise(&caller.roles)
    }

    fn is_platform_admin(&self, caller: &Caller) -> bool {
        may_customise(&caller.roles)
    }
}

/// Sends the audit trail and the lifecycle events to the operator's log. The default grants are
/// derived from the records at every swap, so a node that restarts needs no memory of them.
struct SiteHost {
    site: String,
}

impl GrantSink for SiteHost {
    fn set_default_grants(&self, _: &str, _: &[DefaultGrant]) -> std::result::Result<(), String> {
        Ok(())
    }
}

impl AuditSink for SiteHost {
    fn record(&self, entry: AuditEntry) {
        tracing::info!(
            site = %self.site,
            actor = %entry.actor,
            action = entry.action,
            subject = %entry.subject,
            version = entry.version,
            "customisation audited"
        );
    }
}

impl EventSink for SiteHost {
    fn emit(&self, event: CustomiseEvent) {
        tracing::info!(
            site = %self.site,
            kind = event.kind,
            doctype = %event.doctype,
            version = event.version,
            "customisation live"
        );
    }
}

impl ReferrerLookup for SiteHost {
    fn referrers(&self, _doctype: &str, _field: &str) -> Vec<Referrer> {
        Vec::new()
    }
}

/// The schema step: the online migration engine, over a catalogue that keeps the version
/// store's own table (the plan would otherwise propose to drop it).
struct SiteSchema {
    inner: spot_customise::pg::PgSchema,
    metadata: Arc<LiveMetadata>,
}

impl SiteSchema {
    fn merged(&self, wanted: &Catalog) -> std::result::Result<Catalog, String> {
        self.metadata
            .merge_catalog(wanted)
            .map_err(|e| e.to_string())
    }
}

impl SchemaBackend for SiteSchema {
    fn plan<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, SchemaPlan> {
        Box::pin(async move { self.inner.plan(&self.merged(wanted)?).await })
    }

    fn apply<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, ()> {
        Box::pin(async move { self.inner.apply(&self.merged(wanted)?).await })
    }

    fn has_rows<'a>(&'a self, doctype: &'a str) -> SchemaFuture<'a, bool> {
        self.inner.has_rows(doctype)
    }

    fn backfill<'a>(&'a self, wanted: &'a Catalog, fills: &'a [Fill]) -> SchemaFuture<'a, u64> {
        Box::pin(async move { self.inner.backfill(&self.merged(wanted)?, fills).await })
    }
}

/// Points the engine, the permission snapshot and the served metadata at each new snapshot.
struct Swapper {
    live: Arc<SiteLive>,
    records: Arc<dyn RecordStore>,
    /// The snapshot a swap failed to reach the engine with; retried on the next poll.
    pending: std::sync::Mutex<Option<Arc<Snapshot>>>,
}

impl Swapper {
    /// The default grant of every customised DocType the snapshot holds.
    async fn defaults(
        &self,
        snapshot: &Snapshot,
    ) -> std::result::Result<Vec<spot_permissions::RoleGrant>, String> {
        let records = self.records.load().await?;
        Ok(records
            .doctypes
            .iter()
            .filter(|r| matches!(r.state, RecordState::Applied | RecordState::Pending))
            .filter(|r| snapshot.meta().doctype(&r.name).is_some())
            .map(|r| SiteLive::default_grant(&r.name, SYSTEM_MANAGER))
            .collect())
    }

    async fn serve(&self, snapshot: &Arc<Snapshot>) -> std::result::Result<(), String> {
        let outcome = async {
            let defaults = self.defaults(snapshot).await?;
            self.live
                .swap_to(
                    Arc::clone(snapshot.meta()),
                    snapshot.version().get(),
                    defaults,
                )
                .await
        }
        .await;
        *self.pending.lock().unwrap_or_else(|p| p.into_inner()) =
            outcome.is_err().then(|| Arc::clone(snapshot));
        self.live.set_problem(outcome.clone().err().map(|problem| {
            format!(
                "metadata version {} could not be served: {problem}",
                snapshot.version().get()
            )
        }));
        outcome
    }
}

impl SwapListener for Swapper {
    fn swapped<'a>(&'a self, snapshot: &'a Arc<Snapshot>) -> ListenerFuture<'a> {
        Box::pin(async move { self.serve(snapshot).await })
    }
}

/// One site's runtime customisation.
pub struct Customise {
    service: Arc<CustomiseService>,
    swapper: Arc<Swapper>,
    metadata: Arc<LiveMetadata>,
    principals: Arc<dyn PrincipalSource>,
    poll: Duration,
}

/// How a [`Customise`] is built.
pub struct Settings {
    /// The site's live hub; the engine and permission snapshot are attached to it.
    pub live: Arc<SiteLive>,
    /// The site's live metadata (version store, bus, snapshot).
    pub metadata: Arc<LiveMetadata>,
    /// The site's pooled database.
    pub backend: PgBackend,
    /// The site's catalogue, which holds the three customisation record DocTypes.
    pub catalog: Arc<Catalog>,
    /// Resolves a user to the roles the access check reads.
    pub principals: Arc<dyn PrincipalSource>,
    /// The site name, for the operator's log.
    pub site: String,
    /// How often a node looks for a change another node made.
    pub poll: Duration,
}

impl Customise {
    /// Builds the service and registers its operations and gate on the site's hub.
    pub fn new(settings: Settings) -> Result<Arc<Self>> {
        let Settings {
            live,
            metadata,
            backend,
            catalog,
            principals,
            site,
            poll,
        } = settings;
        let records: Arc<dyn RecordStore> =
            Arc::new(StorageRecordStore::new(Arc::new(backend.clone()), catalog));
        let swapper = Arc::new(Swapper {
            live: Arc::clone(&live),
            records: Arc::clone(&records),
            pending: std::sync::Mutex::new(None),
        });
        let schema = Arc::new(SiteSchema {
            inner: spot_customise::pg::PgSchema::new(Arc::new(backend)),
            metadata: Arc::clone(&metadata),
        });
        let service = CustomiseService::new(Parts {
            site: metadata.site().clone(),
            base: metadata.base_definitions()?,
            options: BuildOptions::new(),
            live: Arc::clone(metadata.live()),
            bus: metadata.bus(),
            versions: metadata.versions(),
            definitions: metadata.definition_sink(),
            records,
            schema,
        })
        .with_access(Arc::new(RoleCheck))
        .with_host(Arc::new(SiteHost { site }))
        .with_listener(swapper.clone());
        let service = Arc::new(service);
        live.stamp_version(metadata.current().version().get());
        live.set_gate(Arc::clone(service.gate()));
        metadata.defer_following();
        let this = Arc::new(Self {
            service,
            swapper,
            metadata,
            principals,
            poll,
        });
        live.set_admin(this.clone());
        Ok(this)
    }

    /// The underlying service.
    pub fn service(&self) -> &Arc<CustomiseService> {
        &self.service
    }

    /// The caller the principal of `user` makes: the roles of their unscoped assignments.
    async fn caller(&self, user: RecordId) -> Caller {
        let principal = self.principals.resolve(&user).await;
        // Customising changes the whole site, so only an assignment with no scope restriction
        // counts: "System Manager for Company B" is not a site administrator.
        let roles: BTreeSet<String> = principal
            .assignments
            .iter()
            .filter(|a| a.scope.is_empty())
            .flat_map(|a| a.roles.iter().map(|r| r.0.clone()))
            .collect();
        Caller {
            user: user.to_string(),
            roles,
        }
    }

    /// Brings this node in line with the shared records when another node announced a newer
    /// version, and retries a swap that failed to reach the engine.
    async fn follow(&self) -> std::result::Result<(), String> {
        let current = self.metadata.current();
        let head = self
            .metadata
            .versions()
            .read()
            .await
            .map_err(|e| e.to_string())?;
        if let Some((version, hash)) = head
            && (version > current.version()
                || (version == current.version() && &hash != current.hash()))
        {
            self.service
                .recover()
                .await
                .map_err(|e| format!("{}: {e}", e.code()))?;
        }
        self.reconcile_served().await
    }

    /// Retries a swap that did not reach the engine, so the old metadata does not serve for
    /// ever after a transient failure.
    async fn reconcile_served(&self) -> std::result::Result<(), String> {
        let pending = self
            .swapper
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        match pending {
            Some(snapshot) => self.swapper.serve(&snapshot).await,
            None => Ok(()),
        }
    }
}

impl Service for Customise {
    fn name(&self) -> &'static str {
        "customise"
    }

    fn level(&self) -> Level {
        // Without it the site would serve base metadata over a database that holds customised
        // tables, and would answer a customised DocType with `unknown`.
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.service
                .recover()
                .await
                .map_err(|e| format!("customisation recovery failed ({}): {e}", e.code()))?;
            self.reconcile_served().await
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(self.poll);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => {
                        if let Err(problem) = self.follow().await {
                            tracing::warn!(
                                site = %self.metadata.site(),
                                %problem,
                                "the customised metadata could not be followed"
                            );
                        }
                    }
                }
            }
        })
    }
}

// ------------------------------------------------------------------------ the admin methods

fn text<'a>(
    args: &'a Json,
    key: &str,
) -> std::result::Result<&'a str, spot_http::api::service::ApiError> {
    args.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| invalid(format!("`{key}` is required and must be text.")))
}

fn optional<'a>(args: &'a Json, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Json::as_str)
        .filter(|v| !v.is_empty())
}

fn invalid(message: String) -> spot_http::api::service::ApiError {
    spot_http::api::service::ApiError::new(
        spot_http::api::registry::Code::ApiInvalidRequest,
        message,
    )
}

/// The API error of a failed customisation: every validation problem at once, with its path.
fn api_error(error: &CustomiseError) -> spot_http::api::service::ApiError {
    use spot_http::api::registry::Code;
    use spot_http::api::service::{ApiError, FieldIssue};
    let code = match error {
        CustomiseError::Forbidden { .. } => Code::PermissionDenied,
        CustomiseError::Invalid { .. } | CustomiseError::Destructive { .. } => {
            Code::DocumentValidationFailed
        }
        CustomiseError::NotFound { .. } => Code::MetaUnknownDoctype,
        CustomiseError::Divergence { .. } => Code::ServerUnavailable,
        _ => Code::HttpInternal,
    };
    let mut api =
        ApiError::new(code, error.to_string()).detail("customise_code", json!(error.code()));
    for issue in error.issues() {
        api = api.with_field(FieldIssue::new(
            issue.path.clone(),
            issue.code,
            issue.message.clone(),
        ));
    }
    if let CustomiseError::Destructive { steps } = error {
        for step in steps {
            let path = match &step.field {
                Some(field) => format!("{}.{field}", step.entity),
                None => step.entity.clone(),
            };
            api = api.with_field(FieldIssue::new(path, error.code(), step.reason.clone()));
        }
    }
    api
}

impl Admin for Customise {
    fn call<'a>(
        &'a self,
        user: RecordId,
        function: &'a str,
        args: Json,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        std::result::Result<Json, spot_http::api::service::ApiError>,
    > {
        Box::pin(async move {
            let caller = self.caller(user).await;
            let applied = match function {
                "create_doctype" => {
                    let definitions: Vec<&str> = match args.get("definitions") {
                        Some(Json::Array(items)) => items
                            .iter()
                            .map(|item| {
                                item.as_str().ok_or_else(|| {
                                    invalid("`definitions` must be a list of text.".into())
                                })
                            })
                            .collect::<std::result::Result<_, _>>()?,
                        _ => vec![text(&args, "definition")?],
                    };
                    self.service.create_doctypes(&caller, &definitions).await
                }
                "update_doctype" => {
                    self.service
                        .update_doctype(&caller, text(&args, "name")?, text(&args, "definition")?)
                        .await
                }
                "add_custom_field" => {
                    let spec: FieldSpec = serde_json::from_value(
                        args.get("field")
                            .cloned()
                            .ok_or_else(|| invalid("`field` is required.".into()))?,
                    )
                    .map_err(|e| invalid(format!("`field` is not a field definition: {e}")))?;
                    self.service
                        .add_custom_field(
                            &caller,
                            text(&args, "doctype")?,
                            &spec,
                            optional(&args, "insert_after"),
                        )
                        .await
                }
                "remove_custom_field" => {
                    self.service
                        .remove_custom_field(&caller, text(&args, "doctype")?, text(&args, "name")?)
                        .await
                }
                "set_property" => {
                    self.service
                        .set_property(
                            &caller,
                            text(&args, "doctype")?,
                            optional(&args, "field"),
                            text(&args, "property")?,
                            text(&args, "value")?,
                        )
                        .await
                }
                "clear_property" => {
                    self.service
                        .clear_property(
                            &caller,
                            text(&args, "doctype")?,
                            optional(&args, "field"),
                            text(&args, "property")?,
                        )
                        .await
                }
                "list" => {
                    let entries = self
                        .service
                        .list(&caller)
                        .await
                        .map_err(|e| api_error(&e))?;
                    return Ok(json!({
                        "entries": entries.iter().map(|entry| json!({
                            "kind": entry.kind,
                            "doctype": entry.doctype,
                            "field": entry.field,
                            "name": entry.name,
                            "state": entry.state.as_str(),
                            "reason": entry.reason,
                            "intent": entry.intent,
                            "actor": entry.actor,
                        })).collect::<Vec<_>>(),
                    }));
                }
                other => {
                    return Err(spot_http::api::service::ApiError::new(
                        spot_http::api::registry::Code::MethodNotFound,
                        format!("There is no customise operation `{other}`."),
                    ));
                }
            };
            let applied = applied.map_err(|e| api_error(&e))?;
            Ok(json!({ "version": applied.version.get() }))
        })
    }
}
