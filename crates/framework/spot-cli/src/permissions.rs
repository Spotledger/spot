//! Per-site permission assembly: one merged model, one snapshot, one storage-backed source of
//! principals, and the engine wrapper that makes list reads obey the same rules.
//!
//! Role and assignment changes are storage records, so this host resolves a user's assignments
//! on every request instead of caching them. A reload rebuilds the metadata and the merged
//! app grants; in-flight requests keep the snapshot they started with.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_api_services::ApiErrorExt;
use spot_api_services::query::QueryService;
use spot_app_api::context::Outcome;
use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::{RecordId, Version};
use spot_app_api::{AppResult, DocTypeDef};
use spot_app_api::{Document, DocumentPatch};
use spot_core::Clock;
use spot_document::engine::{
    Authorizer, Engine, PermissionAuthorizer, PrincipalSource, QueryPlan, apply_masks, empty_page,
};
use spot_http::api::service::{ApiError, BoxFuture};
use spot_meta::EffectiveMeta;
use spot_permissions::{AppliesTo, AssignmentContext, Principal, Role, UserPermission};
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentStore, Expr, Isolation, Name, Query, QueryExecutor, QueryPath, RowLock,
    Selection, StorageBackend, Transaction, TxOptions, Value,
};

use crate::login::system_clock;
use crate::{Error, Result, apps, config, error, schema};

fn name(text: &str) -> Result<Name> {
    Name::new(text).map_err(error)
}

fn options() -> Result<TxOptions> {
    Ok(TxOptions::new(
        name("spot_cli_permissions")?,
        Isolation::ReadCommitted,
    ))
}

/// The named permissions of a site's installed apps and the roles' grants of them.
///
/// Both travel with every snapshot the site builds (the first one, a reload and every
/// customisation swap), so a swap can never drop a grant or forget a declaration.
#[derive(Clone, Debug, Default)]
pub struct AppPerms {
    /// Every permission the installed apps declare. A name belongs to one app.
    pub catalogue: spot_permissions::AppPermissionCatalogue,
    /// The roles' grants of them, from the apps' `permissions/app_permissions.toml`.
    pub grants: Vec<spot_permissions::AppPermissionGrant>,
}

/// The one permission engine a site evaluates: the effective metadata's protection facts over
/// the merged grants of every installed app, and the installed apps' named permissions.
pub fn engine(
    meta: &EffectiveMeta,
    model: &spot_permissions::Model,
    apps: &AppPerms,
) -> Result<Arc<spot_permissions::PermissionEngine>> {
    // A DocType may declare a protection group without an app-level grant table: the group is
    // still closed (nothing opens it), so declare it here rather than rejecting the site.
    let mut model = model.clone();
    for protection in meta.field_protection() {
        for group in &protection.groups {
            if !model
                .groups
                .iter()
                .any(|declared| &declared.name.0 == group)
            {
                model.groups.push(spot_permissions::ProtectionGroup {
                    name: spot_permissions::GroupName(group.clone()),
                    trace: false,
                });
            }
        }
    }
    let describe = |errors: spot_permissions::PermErrors| Error(format!("{errors:?}"));
    let mut snapshot =
        spot_permissions::Snapshot::build_with_apps(meta, &model, &apps.catalogue, &apps.grants)
            .map_err(describe)?;
    for view in meta.doctypes() {
        let name = view.name.as_str();
        let prefix = format!("{name}.");
        let facts: Vec<spot_permissions::FieldFacts> = meta
            .field_protection()
            .iter()
            .filter_map(|p| protection_facts(p, &prefix))
            .collect();
        snapshot = snapshot.with_field_facts(name, facts).map_err(describe)?;
    }
    Ok(Arc::new(spot_permissions::PermissionEngine::new(Arc::new(
        snapshot,
    ))))
}

/// The protection facts of one field, when `protection` belongs to the DocType `prefix` names.
fn protection_facts(
    protection: &spot_meta::FieldProtection,
    prefix: &str,
) -> Option<spot_permissions::FieldFacts> {
    let path = protection.path.strip_prefix(prefix)?;
    Some(spot_permissions::FieldFacts {
        path: path.to_owned(),
        groups: protection
            .groups
            .iter()
            .cloned()
            .map(spot_permissions::GroupName)
            .collect(),
        tags: protection
            .tags
            .iter()
            .cloned()
            .map(spot_permissions::Tag)
            .collect(),
        mask: protection.mask.as_deref().map(|text| {
            text.strip_prefix("last_chars(")
                .and_then(|rest| rest.strip_suffix(')'))
                .and_then(|n| n.parse::<u8>().ok())
                .map_or(
                    spot_permissions::MaskFormat::Placeholder,
                    spot_permissions::MaskFormat::LastChars,
                )
        }),
    })
}

/// Resolves a user id to the assignments the platform's `role_assignment` records hold, each
/// with the scope of its `role_assignment_scope` child rows.
pub struct StoragePrincipals {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    clock: Arc<dyn Clock>,
}

impl StoragePrincipals {
    /// A source over one site's storage.
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            backend,
            catalog,
            clock: system_clock(),
        }
    }

    async fn assignments(
        &self,
        user: &RecordId,
    ) -> std::result::Result<Vec<AssignmentContext>, String> {
        let now = self.clock.now_utc();
        let mut tx = self
            .backend
            .begin(self.catalog.clone(), options().map_err(|e| e.to_string())?)
            .await
            .map_err(|e| e.to_string())?;
        let result = async {
            let entity = name("role_assignment").map_err(|e| e.to_string())?;
            let alias = name("r").map_err(|e| e.to_string())?;
            let mut query = Query::from(&entity, &alias, QueryPath::Report);
            query.select = vec![Selection {
                expr: Expr::field(&alias, &name("id").map_err(|e| e.to_string())?),
                alias: name("v").map_err(|e| e.to_string())?,
            }];
            query.filter = Some(
                Expr::field(&alias, &name("user").map_err(|e| e.to_string())?)
                    .eq(Expr::value(Value::Id(*user))),
            );
            let rows = tx.query(&query).await.map_err(|e| e.to_string())?;
            let ids: Vec<RecordId> = rows
                .rows
                .into_iter()
                .flatten()
                .filter_map(|value| match value {
                    Value::Id(id) => Some(id),
                    _ => None,
                })
                .collect();
            let mut assignments = Vec::new();
            for id in ids {
                let Some(loaded) = tx
                    .load(&entity, id, RowLock::None)
                    .await
                    .map_err(|e| e.to_string())?
                else {
                    continue;
                };
                if !active(&loaded.data.fields, now) {
                    continue;
                }
                let Some(role_id) = link(&loaded.data.fields, "role") else {
                    continue;
                };
                let Some(role) = tx
                    .load(
                        &name("role").map_err(|e| e.to_string())?,
                        role_id,
                        RowLock::None,
                    )
                    .await
                    .map_err(|e| e.to_string())?
                else {
                    continue;
                };
                let Some(role_name) = text(&role.data.fields, "role_name") else {
                    continue;
                };
                let scope = loaded
                    .data
                    .children
                    .iter()
                    .filter(|(table, _)| table.as_str() == "scope")
                    .flat_map(|(_, rows)| rows.iter())
                    .filter_map(|row| permission(&row.fields))
                    .collect();
                assignments.push(AssignmentContext {
                    id: id.to_string(),
                    roles: vec![Role(role_name)],
                    scope,
                });
            }
            assignments.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(assignments)
        }
        .await;
        let _ = tx.rollback().await;
        result
    }
}

impl PrincipalSource for StoragePrincipals {
    fn resolve<'a>(&'a self, user: &'a RecordId) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            let assignments = self.assignments(user).await.unwrap_or_default();
            Principal::user(&user.to_string(), assignments)
        })
    }
}

fn value<'a>(fields: &'a BTreeMap<Name, Value>, field: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(name, value)| (name.as_str() == field).then_some(value))
}

fn text(fields: &BTreeMap<Name, Value>, field: &str) -> Option<String> {
    match value(fields, field) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

fn link(fields: &BTreeMap<Name, Value>, field: &str) -> Option<RecordId> {
    match value(fields, field) {
        Some(Value::Id(id)) => Some(*id),
        _ => None,
    }
}

fn instant(fields: &BTreeMap<Name, Value>, field: &str) -> Option<time::OffsetDateTime> {
    match value(fields, field) {
        Some(Value::DateTime(at)) => Some(*at),
        _ => None,
    }
}

fn active(fields: &BTreeMap<Name, Value>, now: time::OffsetDateTime) -> bool {
    if text(fields, "state").as_deref() != Some("active") {
        return false;
    }
    if instant(fields, "valid_from").is_some_and(|from| from > now) {
        return false;
    }
    if instant(fields, "valid_to").is_some_and(|to| to <= now) {
        return false;
    }
    true
}

fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::Json(text) => serde_json::from_str::<Vec<String>>(text).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn permission(fields: &BTreeMap<Name, Value>) -> Option<UserPermission> {
    let field_or_link_doctype = text(fields, "field_or_link_doctype")?;
    let allowed = value(fields, "allowed").map(strings).unwrap_or_default();
    let doctype = text(fields, "doctype").filter(|d| !d.is_empty());
    let applies_to = match text(fields, "applies_to").as_deref() {
        Some("only") => AppliesTo::Only(
            value(fields, "applies_to_doctypes")
                .map(strings)
                .unwrap_or_default(),
        ),
        _ => AppliesTo::AllDoctypes,
    };
    let include_descendants = matches!(
        value(fields, "include_descendants"),
        Some(Value::Bool(true))
    );
    Some(UserPermission {
        user: String::new(),
        doctype,
        field_or_link_doctype,
        allowed,
        applies_to,
        include_descendants,
    })
}

/// The engine the native API adapters see: document operations go to the real engine (whose
/// `PermissionAuthorizer` decides every one), and list reads compile the principal's read
/// rights into the query before storage is touched.
pub struct GuardedEngine {
    engine: Arc<Engine<PgBackend>>,
    authorizer: Arc<PermissionAuthorizer>,
    ops: Arc<std::sync::atomic::AtomicUsize>,
}

impl GuardedEngine {
    /// Wraps `engine`, counting calls for the host's tests.
    pub fn new(
        engine: Arc<Engine<PgBackend>>,
        authorizer: Arc<PermissionAuthorizer>,
        ops: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            engine,
            authorizer,
            ops,
        }
    }

    fn touched(&self) {
        self.ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl DocumentService for GuardedEngine {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.touched();
        Box::pin(self.engine.meta(doctype))
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.load(inv, doctype, id))
    }

    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.create(inv, doc))
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.update(inv, doctype, id, patch, expected))
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.post(inv, doctype, id, expected))
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.cancel(inv, doctype, id, expected))
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.correct(inv, doctype, id, expected))
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        self.touched();
        Box::pin(self.engine.delete(inv, doctype, id, expected))
    }

    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.rename(inv, doctype, id, new_name, expected))
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: serde_json::Value,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        self.touched();
        Box::pin(
            self.engine
                .call_method(inv, doctype, id, method, args, expected),
        )
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.touched();
        Box::pin(self.engine.field_changed(inv, draft, field))
    }
}

impl QueryService for GuardedEngine {
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, std::result::Result<spot_storage_contract::Rows, ApiError>> {
        self.touched();
        Box::pin(async move {
            let principal = self.authorizer.principal_for(inv).await;
            let plan = self
                .authorizer
                .readable_query(&principal, query)
                .await
                .map_err(ApiErrorExt::into_api_error)?;
            let Some(QueryPlan { query, masks }) = plan else {
                return Ok(empty_page(query));
            };
            let mut rows = self
                .engine
                .backend()
                .read(catalog, spot_storage_contract::ReadSource::Primary, &query)
                .await
                .map_err(|e| {
                    spot_app_api::error::map_storage_error(
                        &e,
                        spot_app_api::error::StorageOp::Other,
                    )
                    .into_api_error()
                })?;
            apply_masks(&mut rows, &masks);
            Ok(rows)
        })
    }
}

/// Builds a site's metadata, merged permission model and authorizer, reporting every problem at
/// once. A site whose snapshot is bad is refused without affecting the other sites.
pub async fn site(
    root: &std::path::Path,
    loaded: &config::Loaded,
    site: &config::Site,
) -> Result<SitePermission> {
    site_serving(root, loaded, site, None, &[]).await
}

/// Like [`site`], for a site that may already serve customised metadata: `serving` is the
/// metadata the running site serves now (a reload starts from it, never from the files alone,
/// or it would drop the DocTypes made at runtime) and `defaults` are the default grants of the
/// customised DocTypes.
pub async fn site_serving(
    root: &std::path::Path,
    loaded: &config::Loaded,
    site: &config::Site,
    serving: Option<Arc<EffectiveMeta>>,
    defaults: &[spot_permissions::RoleGrant],
) -> Result<SitePermission> {
    // The engine's metadata is the installed apps' definitions plus the site's recorded
    // runtime changes, so a custom field added at runtime is served by the next rebuild.
    let meta = match serving {
        Some(meta) => meta,
        None => crate::services::metadata::effective(root, loaded, site)?,
    };
    let mut model = apps::site_model(root, loaded, site)?;
    model.grants.extend(defaults.iter().cloned());
    let app_perms = crate::services::controllers::app_permissions(root, loaded, site)?;
    let engine = engine(&meta, &model, &app_perms)?;
    let backend = schema::backend(loaded, site).await?;
    let catalog = Arc::new(schema::catalog(&meta)?);
    let principals = Arc::new(StoragePrincipals::new(backend, catalog.clone()));
    let authorizer = Arc::new(PermissionAuthorizer::new(
        engine.clone(),
        principals.clone(),
    ));
    // The model the site was built with, without the default grants of customised DocTypes
    // (those are re-derived at every swap).
    let mut base_model = model;
    base_model.grants.retain(|g| !defaults.contains(g));
    Ok(SitePermission {
        meta,
        catalog,
        model: base_model,
        decision: engine,
        principals,
        authorizer,
        apps: app_perms,
    })
}

/// What a site's permission assembly produces.
pub struct SitePermission {
    /// The effective metadata of every installed app, platform first.
    pub meta: Arc<EffectiveMeta>,
    /// The catalogue compiled from that metadata.
    pub catalog: Arc<Catalog>,
    /// The merged app grants the decision core was built from (no runtime default grants).
    pub model: spot_permissions::Model,
    /// The snapshot the engine and the list filter decide with.
    pub decision: Arc<spot_permissions::PermissionEngine>,
    /// The storage-backed user-to-assignments source.
    pub principals: Arc<StoragePrincipals>,
    /// The engine's and the list filter's one decision source.
    pub authorizer: Arc<PermissionAuthorizer>,
    /// The installed apps' named permissions and the roles' grants of them.
    pub apps: AppPerms,
}
