//! Permission enforcement for the document engine (WP-PLT-03.T2; ADR-070 section 12).
//!
//! This module is the engine's **one** dependency on permissions. Everything that decides
//! whether an operation may run, whether a field may be written or seen, whether a link target
//! is in scope and which rows a list may read goes through the object-safe [`Authorizer`] trait.
//! The production implementation, [`PermissionAuthorizer`], wraps the decision core
//! ([`spot_permissions::PermissionEngine`]) and a [`PrincipalSource`] that turns an
//! [`Actor`] into a [`Principal`].
//!
//! # What is enforced, and where
//!
//! The engine calls the authorizer at these points (the call-sites are listed in the PR):
//!
//! - **operation gates** — create, update, delete, post, cancel, correct, rename and whitelisted
//!   method calls ask `can` before any step runs, so a refusal happens inside the transaction
//!   and leaves nothing behind;
//! - **field writes** — `check_write` on the fields a create or update sets, `table[idx].field`
//!   for a child row;
//! - **link scope** — `check_links` on create and update, so a document cannot point at a
//!   record outside the acting user's scope;
//! - **reads** — `field_access` on every load and on the document a write returns, so masked
//!   fields come back masked and unreadable fields are gone;
//! - **lists** — [`spot_perm_query::PermissionFilter`] compiles the principal's read rights into
//!   one filter, checks the query against the no-inference rule and restricts its columns.
//!
//! [`Actor::System`] resolves to [`Principal::system`], which the decision core allows
//! everywhere. A user the [`PrincipalSource`] does not know resolves to a principal with **no
//! assignments**: every check refuses it, and never with an error that reveals whether the user
//! exists.
//!
//! # Telemetry
//!
//! Every refusal the engine raises is counted by the engine's denial log
//! ([`Engine::denial_count`](super::Engine::denial_count)), alongside the retry log.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::lifecycle::Operation;
use spot_app_api::prelude::{Query, RecordId};
use spot_app_api::{AppResult, BoxFuture, DocTypeDef, Document, MethodDecl, MethodRight};
use spot_permissions::{
    Action, Decision, DocFacts, FIELD_DENIED, FieldChange, FieldViolation, PermissionEngine,
    Principal, Reason,
};

use super::Engine;
use super::ops::Ret;
use spot_storage_contract::StorageBackend;

pub mod facts;
mod gate;
mod query;

pub use facts::{WriteChange, create_changes, mask_document, patch_changes};
pub use query::{QueryPlan, apply_masks, empty_page};

/// Turns an [`Actor`] into the [`Principal`] a decision is asked about (the storage-backed
/// implementation is WP-IAM-03's; [`MemoryPrincipals`] is the in-memory fake).
pub trait PrincipalSource: Send + Sync + 'static {
    /// The principal acting as the user `user`. An unknown user resolves to a principal with no
    /// assignments — never to an error, so a check never reveals whether the user exists.
    fn resolve<'a>(&'a self, user: &'a RecordId) -> BoxFuture<'a, Principal>;
}

/// The engine's one dependency on permissions: who is acting, and what they may do.
///
/// Object safe, with boxed futures like the rest of the crate, so a host can swap an
/// implementation at run time (a permission change swaps the decision core's snapshot beneath
/// the same authorizer; nothing is cached across requests).
pub trait Authorizer: Send + Sync + 'static {
    /// The principal acting on `inv`. [`Actor::System`] is [`Principal::system`].
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal>;

    /// The decision for `action` on a document with these facts (for tests and telemetry).
    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, Decision>;

    /// Allows `action` on `doc`, or refuses with `permission.denied`.
    fn authorize<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>>;

    /// Checks the fields `changes` set and the link values `doc` holds for `action`.
    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [WriteChange],
    ) -> BoxFuture<'a, AppResult<()>>;

    /// The document as `principal` may read it: hidden fields gone, masked fields masked.
    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>>;

    /// The query as `principal` may run it; `None` is an empty page (no storage read).
    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>>;

    /// Whether this authorizer enforces nothing at all (the compatibility default).
    fn is_unrestricted(&self) -> bool {
        false
    }
}

/// The production authorizer: the decision core over an immutable snapshot, plus the source of
/// principals. The engine can be **swapped** at run time ([`PermissionAuthorizer::swap`]), so a
/// permission change (a new snapshot) is effective on the next request, with no restart and no
/// decision cached across requests.
pub struct PermissionAuthorizer {
    engine: std::sync::RwLock<Arc<PermissionEngine>>,
    principals: Arc<dyn PrincipalSource>,
}

impl PermissionAuthorizer {
    /// An authorizer over `engine`, resolving actors through `principals`.
    #[must_use]
    pub fn new(engine: Arc<PermissionEngine>, principals: Arc<dyn PrincipalSource>) -> Self {
        Self {
            engine: std::sync::RwLock::new(engine),
            principals,
        }
    }

    /// The decision core in force right now.
    #[must_use]
    pub fn engine(&self) -> Arc<PermissionEngine> {
        self.engine
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the decision core (a permission change). Every request after this reads the
    /// new one; nothing is cached.
    pub fn swap(&self, engine: Arc<PermissionEngine>) {
        *self
            .engine
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = engine;
    }

    fn current(&self) -> Arc<PermissionEngine> {
        self.engine()
    }
}

impl Authorizer for PermissionAuthorizer {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            match &inv.actor {
                Actor::System { .. } => Principal::system(),
                Actor::User { id, .. } => self.principals.resolve(id).await,
                // A consumer holds no role (ADR-083 section 2.2): no assignments, so only a
                // grant that does not need a role can apply.
                Actor::Consumer { .. } => Principal::user(&inv.actor.principal_text(), Vec::new()),
            }
        })
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, Decision> {
        let engine = self.current();
        Box::pin(async move { engine.can(principal, action, facts) })
    }

    fn authorize<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        let engine = self.current();
        Box::pin(async move {
            if principal.is_system {
                return Ok(());
            }
            let facts = facts::facts_of(doc);
            let decision = engine.can(principal, action, &facts);
            if decision.allowed {
                Ok(())
            } else {
                Err(gate::denied(action, doc, &decision))
            }
        })
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        let engine = self.current();
        Box::pin(async move {
            if principal.is_system {
                return Ok(());
            }
            let doctype = doc.doctype().as_str().to_owned();
            let facts = facts::facts_of(doc);
            let op = operation_of(action);

            let top: Vec<(String, FieldChange)> = changes
                .iter()
                .filter(|c| !c.path.contains('['))
                .map(|c| (c.path.clone(), change_of(c)))
                .collect();
            let violations = engine.check_write(principal, &doctype, op, &facts, &top);
            if !violations.is_empty() {
                return Err(gate::field_denied(&violations));
            }

            for change in changes.iter().filter(|c| c.path.contains('[')) {
                let Some((table, index, field)) = child_path(&change.path) else {
                    continue;
                };
                let Some(child) = change.child.as_deref() else {
                    continue;
                };
                let row_facts = child_facts(doc, &table, child, index);
                let violations = engine.check_write(
                    principal,
                    child,
                    op,
                    &row_facts,
                    &[(field, FieldChange::Set)],
                );
                if !violations.is_empty() {
                    return Err(gate::field_denied(&[FieldViolation {
                        path: change.path.clone(),
                        code: FIELD_DENIED,
                    }]));
                }
            }

            let links = engine.check_links(principal, action, &facts, &facts::data_of(doc));
            if !links.is_empty() {
                return Err(gate::link_denied(&links));
            }
            Ok(())
        })
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        let engine = self.current();
        Box::pin(async move {
            if principal.is_system {
                return Ok(doc);
            }
            facts::mask_document(&engine, principal, &doc, def)
        })
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>> {
        let engine = self.current();
        Box::pin(async move {
            if principal.is_system {
                return Ok(Some(QueryPlan {
                    query: query.clone(),
                    masks: Vec::new(),
                }));
            }
            query::plan(&engine, principal, query).map_err(|e| gate::query_denied(&e))
        })
    }
}

/// The compatibility authorizer: enforces nothing. A host that has not yet installed a
/// [`PermissionAuthorizer`] keeps the engine's pre-WP-PLT-03 behaviour. Never mount it on an
/// untrusted surface; the HTTP host installs the real one (WP-PLT-05).
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAuthorizer;

impl Authorizer for AllowAllAuthorizer {
    fn principal_for<'a>(&'a self, _inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        Box::pin(async { Principal::system() })
    }

    fn decide<'a>(
        &'a self,
        _principal: &'a Principal,
        _action: Action,
        _facts: &'a DocFacts,
    ) -> BoxFuture<'a, Decision> {
        Box::pin(async {
            Decision {
                allowed: true,
                reasons: vec![Reason::System],
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        _principal: &'a Principal,
        _action: Action,
        _doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn authorize_write<'a>(
        &'a self,
        _principal: &'a Principal,
        _action: Action,
        _doc: &'a Document,
        _changes: &'a [WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn readable<'a>(
        &'a self,
        _principal: &'a Principal,
        doc: Document,
        _def: &'a DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move { Ok(doc) })
    }

    fn readable_query<'a>(
        &'a self,
        _principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>> {
        Box::pin(async move {
            Ok(Some(QueryPlan {
                query: query.clone(),
                masks: Vec::new(),
            }))
        })
    }

    fn is_unrestricted(&self) -> bool {
        true
    }
}

/// An in-memory [`PrincipalSource`] for tests and single-process hosts: a map from an actor's
/// record id to its principal. An unknown id resolves to a principal with no assignments.
#[derive(Clone, Debug, Default)]
pub struct MemoryPrincipals {
    by_id: BTreeMap<String, Principal>,
}

impl MemoryPrincipals {
    /// No users.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same map with `principal` registered under the actor id `user`.
    #[must_use]
    pub fn with(mut self, user: &RecordId, principal: Principal) -> Self {
        self.by_id.insert(user.to_string(), principal);
        self
    }
}

impl PrincipalSource for MemoryPrincipals {
    fn resolve<'a>(&'a self, user: &'a RecordId) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            self.by_id
                .get(&user.to_string())
                .cloned()
                .unwrap_or_else(|| Principal::user(&user.to_string(), Vec::new()))
        })
    }
}

/// Whether a principal holds an app-registered permission, by name (ADR-16: a whitelisted
/// method may declare `right = Permission(name)`). `spot-permissions` has no such check yet, so
/// the engine owns this seam; a host installs a real one with
/// [`Engine::with_app_permissions`](super::Engine::with_app_permissions).
///
/// The answer is a plain `bool`: an unknown permission name, an unknown principal and an
/// implementation that cannot decide all answer `false`. The default is
/// [`DenyAllAppPermissions`].
pub trait AppPermissionCheck: Send + Sync + 'static {
    /// Whether `principal` holds the app permission `permission`.
    fn holds<'a>(&'a self, principal: &'a Principal, permission: &'a str) -> BoxFuture<'a, bool>;
}

/// The default [`AppPermissionCheck`]: nobody holds any app permission, so a method guarded by
/// one is refused until a host installs a real check (fail closed).
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllAppPermissions;

impl AppPermissionCheck for DenyAllAppPermissions {
    fn holds<'a>(&'a self, _principal: &'a Principal, _permission: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
}

/// An in-memory [`AppPermissionCheck`] (the fake): the permissions each user name holds. A user
/// or a permission it does not list is refused.
#[derive(Clone, Debug, Default)]
pub struct MemoryAppPermissions {
    granted: BTreeMap<String, Vec<String>>,
}

impl MemoryAppPermissions {
    /// Nothing granted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same set with `permission` granted to the principal named `user` (the
    /// [`Principal::user`] name the principal source resolves to).
    #[must_use]
    pub fn grant(mut self, user: &str, permission: &str) -> Self {
        self.granted
            .entry(user.to_owned())
            .or_default()
            .push(permission.to_owned());
        self
    }
}

impl AppPermissionCheck for MemoryAppPermissions {
    fn holds<'a>(&'a self, principal: &'a Principal, permission: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            !principal.user.is_empty()
                && self
                    .granted
                    .get(&principal.user)
                    .is_some_and(|held| held.iter().any(|p| p == permission))
        })
    }
}

/// The lifecycle `Operation` a write check runs under. `Correct` is cancel-and-amend: its
/// amendment is created, so the create right (and the create field right) is the one to ask.
fn operation_of(action: Action) -> Operation {
    match action {
        Action::Op(op) => op,
        Action::Correct => Operation::Create,
        Action::Read => Operation::Update,
    }
}

fn change_of(change: &WriteChange) -> FieldChange {
    if change.set {
        FieldChange::Set
    } else {
        FieldChange::Unchanged
    }
}

/// `table[idx].field` into its three parts.
fn child_path(path: &str) -> Option<(String, usize, String)> {
    let (table, rest) = path.split_once('[')?;
    let (index, field) = rest.split_once("].")?;
    let index: usize = index.parse().ok()?;
    Some((table.to_owned(), index, field.to_owned()))
}

fn child_facts(doc: &Document, table: &str, child: &str, index: usize) -> DocFacts {
    let values = doc
        .tables()
        .iter()
        .find(|(name, _)| name.as_str() == table)
        .and_then(|(_, rows)| rows.get(index.saturating_sub(1)))
        .map(|row| {
            row.fields
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    DocFacts {
        doctype: child.to_owned(),
        values,
        ..DocFacts::default()
    }
}

impl<B: StorageBackend> Engine<B> {
    /// The principal acting on `inv`, without running any check.
    pub(crate) async fn acting_principal(&self, inv: &Invocation) -> Principal {
        self.authorizer.principal_for(inv).await
    }

    /// The document(s) of one operation result, filtered for `principal`: a write result goes
    /// through the same field filter a read does.
    pub(crate) async fn mask_result(&self, principal: &Principal, value: Ret) -> AppResult<Ret> {
        Ok(match value {
            Ret::Doc(doc) => Ret::Doc(self.readable(principal, doc).await?),
            Ret::DocJson(doc, json) => Ret::DocJson(self.readable(principal, doc).await?, json),
            Ret::Unit => Ret::Unit,
        })
    }

    /// Resolves the acting principal and refuses `action` on `doc` unless it is allowed;
    /// returns the principal so the caller can reuse it.
    pub(crate) async fn authorize(
        &self,
        inv: &Invocation,
        action: Action,
        doc: &Document,
    ) -> AppResult<Principal> {
        let principal = self.authorizer.principal_for(inv).await;
        self.authorizer
            .authorize(&principal, action, doc)
            .await
            .map_err(|e| self.denied(e))?;
        Ok(principal)
    }

    /// Refuses the call of a whitelisted method unless the principal holds the right the
    /// method declares (WP-DOC-11.T3), and returns the principal. Fails closed: a right the
    /// engine does not know, or a declaration that contradicts itself, is refused, and an app
    /// permission is held only if the installed [`AppPermissionCheck`] says so. A refusal is
    /// audited in the denial log.
    pub(crate) async fn authorize_method(
        &self,
        inv: &Invocation,
        decl: &MethodDecl,
        doc: &Document,
    ) -> AppResult<Principal> {
        let mut action = Action::Read;
        let mut permission: Option<&str> = None;
        if decl.right.contradicts(decl.mutates) {
            return Err(self.denied(gate::right_unknown(decl.right.as_str(), doc)));
        }
        match &decl.right {
            MethodRight::Read => {}
            MethodRight::Write => action = Action::Op(Operation::Update),
            MethodRight::Create => action = Action::Op(Operation::Create),
            MethodRight::Submit => action = Action::Op(Operation::Post),
            MethodRight::Cancel => action = Action::Op(Operation::Cancel),
            MethodRight::Permission(name) => permission = Some(name.as_str()),
            // A right added to the contract that this engine does not know: refuse.
            other => return Err(self.denied(gate::right_unknown(other.as_str(), doc))),
        }
        let principal = self.authorizer.principal_for(inv).await;
        if let Some(name) = permission {
            // The same system bypass every other gate has; any other principal needs the grant.
            if !principal.is_system && !self.app_permissions.holds(&principal, name).await {
                return Err(self.denied(gate::permission_missing(name, doc)));
            }
        } else {
            self.authorizer
                .authorize(&principal, action, doc)
                .await
                .map_err(|e| self.denied(e))?;
        }
        Ok(principal)
    }

    /// Checks the fields a create/update sets and the link values it holds.
    pub(crate) async fn authorize_write(
        &self,
        principal: &Principal,
        action: Action,
        doc: &Document,
        changes: &[WriteChange],
    ) -> AppResult<()> {
        self.authorizer
            .authorize_write(principal, action, doc, changes)
            .await
            .map_err(|e| self.denied(e))
    }

    /// The document as `principal` may read it.
    pub(crate) async fn readable(
        &self,
        principal: &Principal,
        doc: Document,
    ) -> AppResult<Document> {
        let def = self.snapshot().doctype(doc.doctype().as_str())?;
        self.authorizer
            .readable(principal, doc, &def)
            .await
            .map_err(|e| self.denied(e))
    }

    /// The plan for a list query under the invocation's principal, or `None` for an empty page.
    pub(crate) async fn query_plan(
        &self,
        inv: &Invocation,
        query: &Query,
    ) -> AppResult<Option<QueryPlan>> {
        // The sealed and blind-index columns of a secret are for the secret path only, whoever
        // the principal is (a system principal would otherwise read them in full).
        if crate::secrets::query_names_secret_column(query) {
            return Err(spot_app_api::AppError::new(
                spot_app_api::ErrorClass::PermissionDenied,
                "permission.denied",
                "The query cannot be answered under the user's permissions.",
            )
            .detail("reason", "secret_column"));
        }
        let principal = self.authorizer.principal_for(inv).await;
        self.authorizer
            .readable_query(&principal, query)
            .await
            .map_err(|e| self.denied(e))
    }
}
