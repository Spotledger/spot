//! Quarantine administration hosted (WP-RTB-03.T3): list, inspect, retry, release and discard
//! with audit and a real close guard, plus the drift report and the readiness line. Guide:
//! `docs/running-spot/quarantine.md`.
//!
//! The store is a [`QuarantineBackend`] port (async and fallible: a store that cannot answer is a
//! refusal, never an empty queue); [`backend::StoreBackend`] adapts the library's
//! `QuarantineStore` (the PostgreSQL one of `spot-quarantine-pg` in production) to it, with the
//! real permission engine behind [`guards::EngineCloseGuard`] and the field guard behind
//! [`guards::FieldGuardScope`]. [`GuardedAdmin`] is the admin port on top and holds every rule
//! that must not depend on the store:
//!
//! - a **discard** needs the permission `sync.quarantine.discard` *and* a reason code; the guard
//!   is a [`CloseGuard`] that answers from the real permission engine, never allow-all (an error
//!   from the engine is a refusal);
//! - every action goes through the backend's `transition`: the decision is audited first (no state
//!   change without a recorded decision), and a discard is closed with the library's closure
//!   record, written with the state in one statement (see [`backend`]);
//! - a page is at most 100 items; an item of a DocType the principal cannot read is **counted
//!   but not listed** (`restricted`); a payload view is capped at 1 MiB, a binary image is never
//!   returned whole, and a field the principal may not read is `hidden`.
//!
//! The readiness line is information and never not-ready: `quarantine: 0 items` or
//! `quarantine: 3 items (oldest 2d)`, with the lag when a lag port is wired
//! (`quarantine: 3 items (oldest 2d), lag 0.2s`).

#[path = "quarantine/backend.rs"]
pub mod backend;
#[path = "quarantine/condition.rs"]
pub mod condition;
#[path = "quarantine/guards.rs"]
pub mod guards;
#[path = "quarantine/health.rs"]
pub mod health;
#[path = "quarantine/live.rs"]
pub mod live;

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::Result;

/// The module's name.
pub const NAME: &str = "quarantine";
/// This module's start order: after the sync services.
pub const ORDER: u32 = 1400;
/// The permission a discard needs.
pub const DISCARD_PERMISSION: &str = "sync.quarantine.discard";
/// The most items a list page returns.
pub const MAX_PAGE: usize = 100;
/// The most payload bytes an inspect returns.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// A refusal; the codes are stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The close guard said no (permission missing or the engine failed): fail closed.
    Forbidden,
    /// A discard without a reason code.
    ReasonRequired,
    /// The item does not exist for this tenant (a cross-tenant id answers the same).
    NotFound,
    /// The item is not in a state the action applies to.
    WrongState,
    /// The store could not answer. Nothing was changed.
    Unavailable,
}

impl Refusal {
    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Forbidden => "quarantine_host.forbidden",
            Self::ReasonRequired => "quarantine_host.reason_required",
            Self::NotFound => "quarantine_host.not_found",
            Self::WrongState => "quarantine_host.wrong_state",
            Self::Unavailable => "quarantine_host.unavailable",
        }
    }
}

/// Where an item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for a record it references.
    Parked,
    /// Refused; a person decides.
    Held,
    /// Queued for a retry of the apply path.
    Retrying,
    /// Applied after a retry.
    Closed,
    /// Discarded by a person.
    Discarded,
}

/// One quarantined item, without its image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The item id.
    pub id: String,
    /// The DocType of the held record.
    pub doctype: String,
    /// The reason code.
    pub reason: String,
    /// The node the record came from (for the condition's parameters).
    pub node: String,
    /// The record's apply key as text (for the condition's parameters).
    pub record: String,
    /// The state.
    pub state: State,
    /// Age in seconds.
    pub age_secs: i64,
}

/// The decision record kinds written with each state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A parked item moved to retry.
    Released,
    /// An item was discarded.
    Discarded,
    /// A retry was requested.
    Retried,
}

impl Decision {
    /// The `sys_sync_decision` kind.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Released => "quarantine_released",
            Self::Discarded => "quarantine_discarded",
            Self::Retried => "quarantine_retried",
        }
    }
}

/// The store could not do what was asked; nothing was changed. No detail: the adapter logs its
/// own cause (a driver message can quote a value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The store did not answer, or refused.
    Unavailable,
    /// The item is no longer in the state the action needs (it changed meanwhile).
    Conflict,
}

/// The store, behind a port. Every method is tenant-scoped; an item of another tenant answers as
/// if it did not exist.
#[async_trait]
pub trait QuarantineBackend: Send + Sync {
    /// Open items of `tenant`, oldest first, at most `limit`, after `after`.
    async fn list(
        &self,
        tenant: &str,
        after: Option<&str>,
        limit: usize,
    ) -> std::result::Result<Vec<Item>, BackendError>;
    /// One item.
    async fn get(&self, tenant: &str, id: &str) -> std::result::Result<Option<Item>, BackendError>;
    /// The held image as JSON text (never logged); `None` for a binary or missing image.
    async fn image(
        &self,
        tenant: &str,
        id: &str,
    ) -> std::result::Result<Option<String>, BackendError>;
    /// Changes the state and records the decision.
    async fn transition(
        &self,
        tenant: &str,
        id: &str,
        to: State,
        decision: Decision,
        actor: &str,
        reason: Option<&str>,
    ) -> std::result::Result<(), BackendError>;
    /// Every item's DocType and age, for the counts and the readiness line.
    async fn summary(&self, tenant: &str) -> std::result::Result<Vec<Item>, BackendError>;
}

/// The close guard: the real permission check.
pub trait CloseGuard {
    /// Whether `actor` holds `permission`. An `Err` is a refusal, never an allow.
    ///
    /// # Errors
    ///
    /// A reason when the permission engine fails.
    fn allows(&self, actor: &str, permission: &str) -> std::result::Result<bool, String>;
}

/// Which DocTypes and fields a principal may read.
pub trait ReadScope {
    /// Whether `actor` may read `doctype`.
    fn may_read(&self, actor: &str, doctype: &str) -> bool;
    /// Whether `actor` may read `field` of `doctype`.
    fn may_read_field(&self, actor: &str, doctype: &str, field: &str) -> bool;
}

/// A page of items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// Items the principal may see.
    pub items: Vec<Item>,
    /// Items on the page of DocTypes the principal cannot read: counted, not listed.
    pub restricted: usize,
}

/// The admin port.
pub struct GuardedAdmin<B: QuarantineBackend, G: CloseGuard, S: ReadScope> {
    backend: B,
    guard: G,
    scope: S,
    tenant: String,
}

fn backend_refusal(error: BackendError) -> Refusal {
    match error {
        BackendError::Unavailable => Refusal::Unavailable,
        BackendError::Conflict => Refusal::WrongState,
    }
}

impl<B: QuarantineBackend, G: CloseGuard, S: ReadScope> GuardedAdmin<B, G, S> {
    /// An admin for one tenant.
    pub fn new(backend: B, guard: G, scope: S, tenant: impl Into<String>) -> Self {
        Self {
            backend,
            guard,
            scope,
            tenant: tenant.into(),
        }
    }

    /// The backend (for tests and the host).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Lists a page (at most [`MAX_PAGE`]), hiding items of DocTypes the principal cannot read.
    ///
    /// # Errors
    ///
    /// [`Refusal::Unavailable`] when the store cannot answer (never an empty page).
    pub async fn list(
        &self,
        actor: &str,
        after: Option<&str>,
        limit: usize,
    ) -> std::result::Result<Page, Refusal> {
        let limit = limit.clamp(1, MAX_PAGE);
        let mut items = Vec::new();
        let mut restricted = 0;
        let listed = self
            .backend
            .list(&self.tenant, after, limit)
            .await
            .map_err(backend_refusal)?;
        for item in listed.into_iter().take(MAX_PAGE) {
            if self.scope.may_read(actor, &item.doctype) {
                items.push(item);
            } else {
                restricted += 1;
            }
        }
        Ok(Page { items, restricted })
    }

    /// Inspects an item: metadata plus the image with unreadable top-level fields replaced by
    /// `"hidden"`, capped at [`MAX_PAYLOAD_BYTES`]. A DocType the principal cannot read answers
    /// `NotFound` (it does not reveal that the item exists).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], [`Refusal::Unavailable`].
    pub async fn inspect(
        &self,
        actor: &str,
        id: &str,
    ) -> std::result::Result<(Item, Option<String>), Refusal> {
        let item = self.visible(actor, id).await?;
        let text = self
            .backend
            .image(&self.tenant, id)
            .await
            .map_err(backend_refusal)?;
        let view = text.and_then(|text| {
            let mut value: serde_json::Value = serde_json::from_str(&text).ok()?;
            // A binary (non-object) image is never returned whole.
            let object = value.as_object_mut()?;
            for (field, v) in object.iter_mut() {
                if !self.scope.may_read_field(actor, &item.doctype, field) {
                    *v = serde_json::Value::String("hidden".to_owned());
                }
            }
            let out = value.to_string();
            (out.len() <= MAX_PAYLOAD_BYTES).then_some(out)
        });
        Ok((item, view))
    }

    /// Releases a **parked** item to retry.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], [`Refusal::WrongState`] or [`Refusal::Unavailable`].
    pub async fn release(&self, actor: &str, id: &str) -> std::result::Result<(), Refusal> {
        let item = self.visible(actor, id).await?;
        if item.state != State::Parked {
            return Err(Refusal::WrongState);
        }
        self.backend
            .transition(
                &self.tenant,
                id,
                State::Retrying,
                Decision::Released,
                actor,
                None,
            )
            .await
            .map_err(backend_refusal)
    }

    /// Asks for a retry of a held item.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], [`Refusal::WrongState`] or [`Refusal::Unavailable`].
    pub async fn retry(&self, actor: &str, id: &str) -> std::result::Result<(), Refusal> {
        let item = self.visible(actor, id).await?;
        if !matches!(item.state, State::Held | State::Parked) {
            return Err(Refusal::WrongState);
        }
        self.backend
            .transition(
                &self.tenant,
                id,
                State::Retrying,
                Decision::Retried,
                actor,
                None,
            )
            .await
            .map_err(backend_refusal)
    }

    /// Discards an item: needs [`DISCARD_PERMISSION`] and a reason code.
    ///
    /// # Errors
    ///
    /// [`Refusal::Forbidden`] (also when the permission engine fails),
    /// [`Refusal::ReasonRequired`], [`Refusal::NotFound`], [`Refusal::WrongState`],
    /// [`Refusal::Unavailable`].
    pub async fn discard(
        &self,
        actor: &str,
        id: &str,
        reason_code: &str,
    ) -> std::result::Result<(), Refusal> {
        if !matches!(self.guard.allows(actor, DISCARD_PERMISSION), Ok(true)) {
            return Err(Refusal::Forbidden);
        }
        if reason_code.trim().is_empty() {
            return Err(Refusal::ReasonRequired);
        }
        let item = self.visible(actor, id).await?;
        if matches!(item.state, State::Closed | State::Discarded) {
            return Err(Refusal::WrongState);
        }
        self.backend
            .transition(
                &self.tenant,
                id,
                State::Discarded,
                Decision::Discarded,
                actor,
                Some(reason_code),
            )
            .await
            .map_err(backend_refusal)
    }

    /// Open counts by DocType (items not closed or discarded), and the oldest age.
    ///
    /// # Errors
    ///
    /// [`Refusal::Unavailable`] when the store cannot answer.
    pub async fn counts(
        &self,
    ) -> std::result::Result<(BTreeMap<String, usize>, Option<i64>), Refusal> {
        let mut by_doctype = BTreeMap::new();
        let mut oldest: Option<i64> = None;
        for item in self
            .backend
            .summary(&self.tenant)
            .await
            .map_err(backend_refusal)?
        {
            if matches!(item.state, State::Closed | State::Discarded) {
                continue;
            }
            *by_doctype.entry(item.doctype).or_insert(0) += 1;
            oldest = Some(oldest.map_or(item.age_secs, |o| o.max(item.age_secs)));
        }
        Ok((by_doctype, oldest))
    }

    /// The readiness line. A store that cannot answer says so; it never reports an empty queue.
    pub async fn readiness(&self) -> String {
        match self.counts().await {
            Ok((counts, oldest)) => {
                let total: usize = counts.values().sum();
                match oldest {
                    Some(age) if total > 0 => {
                        format!("quarantine: {total} items (oldest {})", span(age))
                    }
                    _ => "quarantine: 0 items".to_owned(),
                }
            }
            Err(_) => "quarantine: unavailable".to_owned(),
        }
    }

    async fn visible(&self, actor: &str, id: &str) -> std::result::Result<Item, Refusal> {
        let item = self
            .backend
            .get(&self.tenant, id)
            .await
            .map_err(backend_refusal)?
            .ok_or(Refusal::NotFound)?;
        if self.scope.may_read(actor, &item.doctype) {
            Ok(item)
        } else {
            Err(Refusal::NotFound)
        }
    }
}

/// `2d`, `5h`, `7m` from seconds.
pub fn span(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    if minutes >= 48 * 60 {
        format!("{}d", minutes / (24 * 60))
    } else if minutes >= 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

/// `[services.quarantine]`.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Whether the module provides the backend port (default `true`).
    pub enabled: bool,
    /// Whether the module also provides the library's `QuarantineStore` port, the same store the
    /// backend is built over (default `true`). A site that wants the backend only sets it
    /// `false`. Ignored when `enabled` is `false`.
    pub provide_store: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            provide_store: true,
        }
    }
}

/// Provides the module's ports into `ctx`: the backend always, the store when one is given.
///
/// The store is provided after the backend, so a second provider of either port is refused by
/// the context with its message naming both modules.
///
/// # Errors
///
/// A port that another module already provides.
pub fn provide_ports(
    ctx: &mut crate::services::SiteCtx<'_>,
    backend: std::sync::Arc<dyn QuarantineBackend>,
    store: Option<std::sync::Arc<dyn spot_quarantine::QuarantineStore>>,
) -> Result<()> {
    ctx.provide::<dyn QuarantineBackend>(backend)?;
    if let Some(store) = store {
        ctx.provide::<dyn spot_quarantine::QuarantineStore>(store)?;
    }
    Ok(())
}

/// Provides `Arc<dyn QuarantineBackend>` and, unless `provide_store = false`,
/// `Arc<dyn QuarantineStore>` on a site that has a PostgreSQL backend: the
/// `PgQuarantineStore` over the site's pool (one instance, shared by both ports), the platform
/// audit sink ([`live::PlatformAudit`]), the site's permission engine and a principal source over
/// the site's role assignments ([`live::SitePrincipals`]), for the site's own tenant. The
/// consumers of the backend are the remedy host's quarantine part, the ops signals and the admin;
/// the consumer of the store is the edge intake (`sync_node_intake`), which holds back a record
/// the apply path refuses.
///
/// `PgQuarantineStore::prepare` stays the creator of `spot_quarantine_item` and
/// `spot_quarantine_payload` (the migrator does not own them): the site's database role needs DDL
/// rights, and when `prepare` fails **both ports are absent** and the coded log says so.
///
/// The module is optional. A context with no site, a disabled module, or a store whose tables
/// cannot be prepared provides nothing and says so in the log; it never stops the site. A reload
/// provides the backend and the store built by the first build.
///
/// # Errors
///
/// A setting that does not decode, or a port provided twice.
pub async fn register(ctx: &mut crate::services::SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let site = ctx.label().to_owned();
    if !settings.enabled {
        live::forget(&site);
        tracing::debug!("quarantine: disabled by [services.quarantine]; no backend is provided");
        return Ok(());
    }
    if !ctx.assembling() {
        if let Some(existing) = live::recall(&site) {
            let store = if settings.provide_store {
                live::recall_store(&site)
            } else {
                None
            };
            provide_ports(ctx, existing, store)?;
        }
        return Ok(());
    }
    live::forget(&site);
    let (tenant, pool, engine, principals) = match ctx.env() {
        Ok(env) => (
            env.site.site.clone(),
            env.backend.pool(),
            std::sync::Arc::clone(&env.permission.decision),
            std::sync::Arc::clone(&env.permission.principals)
                as std::sync::Arc<dyn spot_document::engine::PrincipalSource>,
        ),
        Err(_) => {
            tracing::info!(
                "quarantine: the site has no PostgreSQL backend; no backend is provided"
            );
            return Ok(());
        }
    };
    let store = std::sync::Arc::new(spot_quarantine_pg::PgQuarantineStore::new(pool.clone()));
    if store.prepare().await.is_err() {
        tracing::warn!(
            code = "quarantine_host.unavailable",
            "quarantine: the store could not be prepared; no backend and no store are provided"
        );
        return Ok(());
    }
    let shared: std::sync::Arc<dyn spot_quarantine::QuarantineStore> = store;
    let backend = live::backend_over(
        &tenant,
        std::sync::Arc::clone(&shared),
        std::sync::Arc::new(live::PlatformAudit::new(
            spot_audit_store::PgAuditStore::new(pool),
            &tenant,
        )),
        engine,
        std::sync::Arc::new(live::SitePrincipals::new(principals)),
        std::sync::Arc::clone(ctx.clock()),
    );
    live::remember(&site, std::sync::Arc::clone(&backend));
    let store_port = if settings.provide_store {
        live::remember_store(&site, std::sync::Arc::clone(&shared));
        Some(shared)
    } else {
        None
    };
    provide_ports(ctx, backend, store_port)
}
