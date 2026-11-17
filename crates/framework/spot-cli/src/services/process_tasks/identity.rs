//! The live site's implementations of the seams: identity reads over the storage contract, the
//! sign-in service, the principals, the guarded document's facts and the served permission engine.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use spot_app_api::context::DocumentService;
use spot_app_api::prelude::SiteId;
use spot_bpm_documents::ids::parse_record_id;
use spot_bpm_tasks::{Directory, DocFactsSource, DocumentServiceFacts, PrincipalSource, Task};
use spot_core::Clock;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_http::api::ports::task::TaskPortError;
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_permissions::{DocFacts, PermissionEngine, Principal};
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentStore, Expr, Isolation, Name, Query, QueryExecutor, QueryPath, RecordId,
    RowLock, Selection, StorageBackend, Transaction, TxOptions, Value,
};
use time::OffsetDateTime;
use tokio::runtime::Handle;

use super::seams::{Callers, DecisionSource, TaskDirectory};
use super::threads::Hop;
use crate::audit::PathClass;
use crate::live::SiteLive;

/// The identity reads of one site: `role`, `role_assignment` and `user` records, read through the
/// storage contract's typed query with bound values, never composed text.
#[derive(Clone)]
struct StorageRead {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    clock: Arc<dyn Clock>,
}

type ReadResult<T> = std::result::Result<T, String>;

fn value_of<'a>(fields: &'a BTreeMap<Name, Value>, field: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(name, value)| (name.as_str() == field).then_some(value))
}

fn text_of(fields: &BTreeMap<Name, Value>, field: &str) -> Option<String> {
    match value_of(fields, field) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

fn link_of(fields: &BTreeMap<Name, Value>, field: &str) -> Option<RecordId> {
    match value_of(fields, field) {
        Some(Value::Id(id)) => Some(*id),
        _ => None,
    }
}

fn instant_of(fields: &BTreeMap<Name, Value>, field: &str) -> Option<OffsetDateTime> {
    match value_of(fields, field) {
        Some(Value::DateTime(at)) => Some(*at),
        _ => None,
    }
}

/// Whether a `role_assignment` is in force at `now`.
fn assignment_active(fields: &BTreeMap<Name, Value>, now: OffsetDateTime) -> bool {
    if text_of(fields, "state").as_deref() != Some("active") {
        return false;
    }
    if instant_of(fields, "valid_from").is_some_and(|from| from > now) {
        return false;
    }
    if instant_of(fields, "valid_to").is_some_and(|to| to <= now) {
        return false;
    }
    true
}

fn user_active(fields: &BTreeMap<Name, Value>) -> bool {
    text_of(fields, "state").as_deref() == Some("active")
}

fn name_of(text: &str) -> ReadResult<Name> {
    Name::new(text).map_err(|e| e.to_string())
}

async fn ids_where(
    tx: &mut spot_postgres::PgTransaction,
    entity: &str,
    field: &str,
    value: Value,
) -> ReadResult<Vec<RecordId>> {
    let entity = name_of(entity)?;
    let alias = name_of("r")?;
    let mut query = Query::from(&entity, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &name_of("id")?),
        alias: name_of("v")?,
    }];
    query.filter = Some(Expr::field(&alias, &name_of(field)?).eq(Expr::value(value)));
    let rows = tx.query(&query).await.map_err(|e| e.to_string())?;
    Ok(rows
        .rows
        .into_iter()
        .flatten()
        .filter_map(|value| match value {
            Value::Id(id) => Some(id),
            _ => None,
        })
        .collect())
}

async fn fields_of(
    tx: &mut spot_postgres::PgTransaction,
    entity: &str,
    id: RecordId,
) -> ReadResult<Option<BTreeMap<Name, Value>>> {
    let entity = name_of(entity)?;
    let loaded = tx
        .load(&entity, id, RowLock::None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(loaded.map(|loaded| loaded.data.fields))
}

impl StorageRead {
    async fn begin(&self) -> ReadResult<spot_postgres::PgTransaction> {
        let options = TxOptions::new(
            name_of("process_tasks_directory")?,
            Isolation::ReadCommitted,
        );
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| e.to_string())
    }

    /// The active users who hold `role` through an assignment in force now.
    async fn users_in_role(&self, role: &str) -> ReadResult<Vec<String>> {
        let now = self.clock.now_utc();
        let mut tx = self.begin().await?;
        let result: ReadResult<Vec<String>> = async {
            let mut users = BTreeSet::new();
            for role_id in
                ids_where(&mut tx, "role", "role_name", Value::Text(role.to_owned())).await?
            {
                for assignment in
                    ids_where(&mut tx, "role_assignment", "role", Value::Id(role_id)).await?
                {
                    let Some(fields) = fields_of(&mut tx, "role_assignment", assignment).await?
                    else {
                        continue;
                    };
                    if !assignment_active(&fields, now) {
                        continue;
                    }
                    let Some(user_id) = link_of(&fields, "user") else {
                        continue;
                    };
                    let Some(user) = fields_of(&mut tx, "user", user_id).await? else {
                        continue;
                    };
                    if user_active(&user) {
                        users.insert(user_id.to_string());
                    }
                }
            }
            Ok(users.into_iter().collect())
        }
        .await;
        let _ = tx.rollback().await;
        result
    }

    /// The roles `user` holds through assignments in force now.
    async fn roles_of(&self, user: RecordId) -> ReadResult<Vec<String>> {
        let now = self.clock.now_utc();
        let mut tx = self.begin().await?;
        let result: ReadResult<Vec<String>> = async {
            let mut roles = BTreeSet::new();
            for assignment in ids_where(&mut tx, "role_assignment", "user", Value::Id(user)).await?
            {
                let Some(fields) = fields_of(&mut tx, "role_assignment", assignment).await? else {
                    continue;
                };
                if !assignment_active(&fields, now) {
                    continue;
                }
                let Some(role_id) = link_of(&fields, "role") else {
                    continue;
                };
                let Some(role) = fields_of(&mut tx, "role", role_id).await? else {
                    continue;
                };
                if let Some(role_name) = text_of(&role, "role_name") {
                    roles.insert(role_name);
                }
            }
            Ok(roles.into_iter().collect())
        }
        .await;
        let _ = tx.rollback().await;
        result
    }

    /// Whether `user` exists and is active.
    async fn is_active(&self, user: RecordId) -> ReadResult<bool> {
        let mut tx = self.begin().await?;
        let result: ReadResult<bool> = async {
            Ok(fields_of(&mut tx, "user", user)
                .await?
                .is_some_and(|fields| user_active(&fields)))
        }
        .await;
        let _ = tx.rollback().await;
        result
    }

    /// The active manager of `user`, when the user is active and has one.
    async fn manager_of(&self, user: RecordId) -> ReadResult<Option<String>> {
        let mut tx = self.begin().await?;
        let result: ReadResult<Option<String>> = async {
            let Some(fields) = fields_of(&mut tx, "user", user).await? else {
                return Ok(None);
            };
            if !user_active(&fields) {
                return Ok(None);
            }
            let Some(manager) = link_of(&fields, "manager") else {
                return Ok(None);
            };
            let active = fields_of(&mut tx, "user", manager)
                .await?
                .is_some_and(|fields| user_active(&fields));
            Ok(active.then(|| manager.to_string()))
        }
        .await;
        let _ = tx.rollback().await;
        result
    }

    /// The active users whose manager is `manager`.
    async fn reports_of(&self, manager: RecordId) -> ReadResult<Vec<String>> {
        let mut tx = self.begin().await?;
        let result: ReadResult<Vec<String>> = async {
            let mut reports = BTreeSet::new();
            for id in ids_where(&mut tx, "user", "manager", Value::Id(manager)).await? {
                if fields_of(&mut tx, "user", id)
                    .await?
                    .is_some_and(|fields| user_active(&fields))
                {
                    reports.insert(id.to_string());
                }
            }
            Ok(reports.into_iter().collect())
        }
        .await;
        let _ = tx.rollback().await;
        result
    }
}

/// The site's directory: [`StorageRead`] reached from plain threads through [`Hop`].
pub struct StorageDirectory {
    read: StorageRead,
    hop: Hop,
}

impl StorageDirectory {
    /// A directory over one site's storage.
    #[must_use]
    pub fn new(
        backend: PgBackend,
        catalog: Arc<Catalog>,
        clock: Arc<dyn Clock>,
        runtime: Handle,
    ) -> Self {
        Self {
            read: StorageRead {
                backend,
                catalog,
                clock,
            },
            hop: Hop::new(runtime),
        }
    }

    /// Runs one read; a failed read is logged without its text and answers `None`.
    fn ask<T, F>(&self, what: &'static str, future: F) -> Option<T>
    where
        T: Send + 'static,
        F: Future<Output = ReadResult<T>> + Send + 'static,
    {
        match self.hop.call(future) {
            Some(Ok(value)) => Some(value),
            Some(Err(_)) | None => {
                tracing::warn!(read = what, "a directory read failed; answering nobody");
                None
            }
        }
    }
}

impl Directory for StorageDirectory {
    fn users_in_role(&self, role: &str) -> Vec<String> {
        let read = self.read.clone();
        let role = role.to_owned();
        self.ask(
            "users_in_role",
            async move { read.users_in_role(&role).await },
        )
        .unwrap_or_default()
    }

    fn manager_of(&self, user: &str) -> Option<String> {
        let id = parse_record_id(user)?;
        let read = self.read.clone();
        self.ask("manager_of", async move { read.manager_of(id).await })
            .flatten()
    }
}

impl TaskDirectory for StorageDirectory {
    fn is_active(&self, user: &str) -> bool {
        let Some(id) = parse_record_id(user) else {
            return false;
        };
        let read = self.read.clone();
        self.ask("is_active", async move { read.is_active(id).await })
            .unwrap_or(false)
    }

    fn reports_of(&self, manager: &str) -> Option<Vec<String>> {
        let id = parse_record_id(manager)?;
        let read = self.read.clone();
        self.ask("reports_of", async move { read.reports_of(id).await })
    }

    fn roles_of(&self, user: &str) -> Option<Vec<String>> {
        let id = parse_record_id(user)?;
        let read = self.read.clone();
        self.ask("roles_of", async move { read.roles_of(id).await })
    }
}

/// The signed-in user of a request: the site's sign-in service resolves the bearer credential.
pub(super) struct LiveCallers {
    pub(super) live: Arc<SiteLive>,
    pub(super) site: String,
}

impl Callers for LiveCallers {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<String, TaskPortError>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.site {
                return Err(TaskPortError::Unauthenticated);
            }
            let auth = self.live.auth().ok_or(TaskPortError::Unavailable)?;
            let who = match auth.authenticate_on(ctx, PathClass::Api).await {
                Ok(who) => who,
                Err(error) => {
                    return Err(if error.code.as_str() == "server.unavailable" {
                        TaskPortError::Unavailable
                    } else {
                        TaskPortError::Unauthenticated
                    });
                }
            };
            if auth.is_restricted(&who) {
                return Err(TaskPortError::PermissionDenied);
            }
            Ok(who.user_id.to_string())
        })
    }
}

/// A user's principal from the site's role assignments; an unreadable one has no rights.
pub(super) struct LivePrincipals {
    pub(super) live: Arc<SiteLive>,
    pub(super) hop: Hop,
}

impl PrincipalSource for LivePrincipals {
    fn principal(&self, user: &str) -> Principal {
        let none = || Principal::user(user, Vec::new());
        let Some(id) = parse_record_id(user) else {
            return none();
        };
        let Some(principals) = self.live.principals() else {
            return none();
        };
        self.hop
            .call(async move { principals.resolve(&id).await })
            .unwrap_or_else(none)
    }
}

/// The guarded document's facts, read through the live document service as the platform itself
/// (the task names `(doctype, id)` only; the read decides nothing about the caller).
pub(super) struct LiveFacts {
    pub(super) live: Arc<SiteLive>,
    pub(super) site: SiteId,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) hop: Hop,
}

impl DocFactsSource for LiveFacts {
    fn facts<'a>(&'a self, task: &'a Task) -> spot_bpm_tasks::AccessFuture<'a, Option<DocFacts>> {
        Box::pin(async move {
            let engine = self.live.engine()?;
            let service: Arc<dyn DocumentService> = engine;
            let clock = Arc::clone(&self.clock);
            let facts = DocumentServiceFacts::new(
                service,
                self.site.clone(),
                Arc::new(move || clock.now_utc()),
            );
            let task = task.clone();
            self.hop
                .call_async(async move { facts.facts(&task).await })
                .await
                .flatten()
        })
    }
}

/// The permission engine of the snapshot the site serves now.
pub(super) struct LiveDecisions {
    pub(super) live: Arc<SiteLive>,
}

impl DecisionSource for LiveDecisions {
    fn engine(&self) -> Option<Arc<PermissionEngine>> {
        self.live
            .served()
            .map(|served| Arc::clone(&served.decision))
    }
}
