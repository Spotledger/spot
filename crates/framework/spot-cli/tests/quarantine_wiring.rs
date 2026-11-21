//! The quarantine module's wiring (WP-AI-07.T8): the real assembly over PostgreSQL
//! (`PgQuarantineStore`, the permission engine, the site principal source, an audit sink) is
//! provided as the `QuarantineBackend` port; a held item is listed through it, a retry records its
//! audit entry first, an audit failure leaves the item unchanged, an unknown actor is refused,
//! another tenant sees nothing, and the remedy host's quarantine part retries through the
//! provided port. The platform audit sink is exercised over the audit store. A site without a
//! database provides no port. Skips (printing a reason) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_audit_chain::AuditStore;
use spot_audit_store::PgAuditStore;
use spot_cli::services::SiteCtx;
use spot_cli::services::controllers::platform_permissions;
use spot_cli::services::quarantine::live::{PlatformAudit, SitePrincipals, backend_over, scope_of};
use spot_cli::services::quarantine::{self, BackendError, Decision, QuarantineBackend, State};
use spot_cli::services::world_access::quarantine::from_parts;
use spot_core::{Clock, ManualClock};
use spot_document::engine::PrincipalSource as SitePrincipalSource;
use spot_http::api::service::BoxFuture;
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::{
    AssignmentContext, Model, Principal, Right, Rights, Role, RoleGrant,
};
use spot_permissions::{
    AppPermissionCatalogue, AppPermissionGrant, AppPermissionName, PermissionEngine, Snapshot,
};
use spot_postgres::testing::TestDb;
use spot_quarantine::{
    AuditAction, AuditEntry, ItemId, NewItem, QuarantineAudit, QuarantineError, QuarantineStore,
    RecordRef, RecordingAudit,
};
use spot_quarantine_pg::PgQuarantineStore;
use spot_remedies::{QuarantineRef, QuarantineState};
use spot_replica_apply::ReceivedRecord;
use spot_storage_contract::RecordId;
use spot_sync_protocol::{RecordName, SourceId};
use time::OffsetDateTime;
use tokio_postgres::NoTls;

fn pool(db: &TestDb) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(PoolConfig::new(4));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

fn perm() -> AppPermissionName {
    AppPermissionName::parse("sync_quarantine_discard").unwrap()
}

/// An engine where `Reviewer` holds the discard permission and the `delete` right on `note`.
fn engine() -> Arc<PermissionEngine> {
    let mut note = Definition::new("note");
    note.fields = vec![FieldSpec::new("title", FieldType::Text)];
    let meta = Registry::new(vec![note]).unwrap().effective();
    let model = Model {
        grants: vec![RoleGrant {
            doctype: "note".to_owned(),
            role: Role("Reviewer".to_owned()),
            rights: Rights::new([Right::Read, Right::Delete]),
            if_owner: false,
        }],
        field_grants: vec![],
        groups: vec![],
        shares: vec![],
    };
    let mut catalogue = AppPermissionCatalogue::new();
    let (_, decl) = platform_permissions().remove(0);
    catalogue.insert(perm(), decl).unwrap();
    let grants = vec![AppPermissionGrant {
        role: Role("Reviewer".to_owned()),
        permission: perm(),
    }];
    let snapshot = Snapshot::build_with_apps(&meta, &model, &catalogue, &grants).unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// The site's principal source: one user holds the `Reviewer` role, every other id holds nothing.
struct Site {
    reviewer: RecordId,
}

impl SitePrincipalSource for Site {
    fn resolve<'a>(&'a self, user: &'a RecordId) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            if *user == self.reviewer {
                Principal::user(
                    &user.to_string(),
                    vec![AssignmentContext::unscoped("a1", &["Reviewer"])],
                )
            } else {
                Principal::user(&user.to_string(), vec![])
            }
        })
    }
}

fn record(entity: &str, seq: u64) -> ReceivedRecord {
    let id = RecordId::new_v7();
    let payload = serde_json::json!({
        "meta_version": 1,
        "document": {
            "entity": entity,
            "id": id.to_string(),
            "fields": { "title": { "t": "text", "v": "hello" } }
        }
    })
    .to_string();
    ReceivedRecord::from_payload(
        &SourceId::new("edge-1#b1").unwrap(),
        RecordName::new(&format!("{entity}:{id}")).unwrap(),
        1,
        seq,
        &payload,
    )
    .unwrap()
}

async fn put(store: &PgQuarantineStore, seq: u64) -> ItemId {
    let rec = record("note", seq);
    let new = NewItem::held(
        rec.source.clone(),
        rec.seq,
        RecordRef::of(&rec),
        "replica_apply.invalid",
        "the shape was refused",
        1_000_000,
    );
    store.insert(new, rec).await.unwrap().id
}

/// An audit sink that can be made to fail.
struct Audit {
    inner: RecordingAudit,
    fail: Mutex<bool>,
}

impl QuarantineAudit for Audit {
    fn record<'a>(&'a self, entry: &'a AuditEntry) -> spot_quarantine::audit::AuditFuture<'a> {
        Box::pin(async move {
            if *self.fail.lock().unwrap() {
                return Err(QuarantineError::Audit {
                    why: "the audit sink is down".to_owned(),
                });
            }
            self.inner.record(entry).await
        })
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
    ))
}

struct Fixture {
    _db: TestDb,
    store: Arc<PgQuarantineStore>,
    audit: Arc<Audit>,
    reviewer: RecordId,
    /// The site's context with the backend provided the way the module provides it.
    ctx: SiteCtx<'static>,
}

async fn fixture() -> Option<Fixture> {
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let pool = pool(&db);
    let store = Arc::new(PgQuarantineStore::new(pool.clone()));
    store.prepare().await.unwrap();
    let audit = Arc::new(Audit {
        inner: RecordingAudit::new(),
        fail: Mutex::new(false),
    });
    let reviewer = RecordId::new_v7();
    let principals = SitePrincipals::new(Arc::new(Site { reviewer }));
    let backend = backend_over(
        "acme",
        Arc::clone(&store) as Arc<dyn QuarantineStore>,
        Arc::clone(&audit) as Arc<dyn QuarantineAudit>,
        engine(),
        Arc::new(principals),
        clock(),
    );
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    ctx.as_module(quarantine::NAME, quarantine::ORDER);
    ctx.provide::<dyn QuarantineBackend>(backend).unwrap();
    Some(Fixture {
        _db: db,
        store,
        audit,
        reviewer,
        ctx,
    })
}

fn provided(f: &Fixture) -> Arc<dyn QuarantineBackend> {
    f.ctx
        .port::<dyn QuarantineBackend>()
        .expect("the port is provided")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_port_is_provided_and_lists_a_held_item() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let backend = provided(&f);
    let items = backend.list("acme", None, 10).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, id.0.to_string());
    assert_eq!(items[0].doctype, "note");
    assert_eq!(items[0].state, State::Held);
    let one = backend.get("acme", &id.0.to_string()).await.unwrap();
    assert!(one.is_some());
    let summary = backend.summary("acme").await.unwrap();
    assert_eq!(summary.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_as_an_actor_records_the_audit_entry_and_moves_the_item() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let text = id.0.to_string();
    provided(&f)
        .transition(
            "acme",
            &text,
            State::Retrying,
            Decision::Retried,
            "agent:agent-1",
            None,
        )
        .await
        .unwrap();
    let entries = f.audit.inner.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, AuditAction::Retried);
    assert_eq!(entries[0].actor, "agent:agent-1");
    assert_eq!(entries[0].item, id);
    let item = f.store.item(id).await.unwrap().unwrap();
    assert_eq!(item.state, spot_quarantine::State::Retrying);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_audit_failure_leaves_the_item_unchanged() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    *f.audit.fail.lock().unwrap() = true;
    let refused = provided(&f)
        .transition(
            "acme",
            &id.0.to_string(),
            State::Retrying,
            Decision::Retried,
            "agent:agent-1",
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(refused, BackendError::Unavailable);
    let item = f.store.item(id).await.unwrap().unwrap();
    assert_eq!(
        item.state,
        spot_quarantine::State::Held,
        "no decision, no effect"
    );
    assert!(f.audit.inner.entries().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_actor_cannot_discard_and_a_reviewer_can() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let text = id.0.to_string();
    let backend = provided(&f);
    // A name that is not a record id, an id the site holds no assignment for, and `system`.
    let stranger = RecordId::new_v7().to_string();
    for actor in ["nobody", stranger.as_str(), "system"] {
        let refused = backend
            .transition(
                "acme",
                &text,
                State::Discarded,
                Decision::Discarded,
                actor,
                Some("duplicate"),
            )
            .await
            .unwrap_err();
        assert_eq!(refused, BackendError::Conflict, "{actor}");
    }
    assert_eq!(
        f.store.item(id).await.unwrap().unwrap().state,
        spot_quarantine::State::Held
    );
    assert!(f.audit.inner.entries().is_empty(), "nothing was audited");
    // The reviewer (as an agent actor: its own role set) discards.
    let actor = format!("agent:{}", f.reviewer);
    backend
        .transition(
            "acme",
            &text,
            State::Discarded,
            Decision::Discarded,
            &actor,
            Some("duplicate"),
        )
        .await
        .unwrap();
    assert_eq!(
        f.store.item(id).await.unwrap().unwrap().state,
        spot_quarantine::State::Discarded
    );
    assert_eq!(f.audit.inner.entries().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_tenants_item_is_invisible() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let text = id.0.to_string();
    let backend = provided(&f);
    assert!(backend.list("globex", None, 10).await.unwrap().is_empty());
    assert!(backend.get("globex", &text).await.unwrap().is_none());
    assert!(backend.summary("globex").await.unwrap().is_empty());
    let refused = backend
        .transition(
            "globex",
            &text,
            State::Retrying,
            Decision::Retried,
            "agent:a",
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(refused, BackendError::Conflict);
    assert_eq!(
        f.store.item(id).await.unwrap().unwrap().state,
        spot_quarantine::State::Held
    );
}

fn blocking<R: Send + 'static>(
    work: impl FnOnce() -> R + Send + 'static,
) -> tokio::task::JoinHandle<R> {
    tokio::task::spawn_blocking(work)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_remedy_hosts_quarantine_part_retries_through_the_provided_port() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let text = id.0.to_string();
    let part = from_parts(
        f.ctx.port::<dyn QuarantineBackend>(),
        "acme",
        Some(tokio::runtime::Handle::current()),
    )
    .expect("a part over the provided port");
    let (held, change) = {
        let part = Arc::clone(&part);
        let text = text.clone();
        blocking(move || {
            let mut ops = part.live_as("agent:agent-1");
            let held = ops.held();
            let change = ops.retry(&QuarantineRef::new(&text)).unwrap();
            (held, change)
        })
        .await
        .unwrap()
    };
    assert_eq!(held, vec![QuarantineRef::new(&text)]);
    assert_eq!(change.before, QuarantineState::Held);
    assert_eq!(change.after, QuarantineState::Retrying);
    assert_eq!(
        f.store.item(id).await.unwrap().unwrap().state,
        spot_quarantine::State::Retrying
    );
    let entries = f.audit.inner.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].actor, "agent:agent-1");
    // The retried item is no longer held.
    let still_held = blocking(move || part.live().held().len()).await.unwrap();
    assert_eq!(still_held, 0, "the retried item is no longer held");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_platform_audit_sink_appends_each_decision_to_its_chain() {
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let pool = pool(&db);
    let store = PgAuditStore::new(pool.clone());
    store.prepare().await.unwrap();
    let quarantine_store = PgQuarantineStore::new(pool.clone());
    quarantine_store.prepare().await.unwrap();
    let id = put(&quarantine_store, 1).await;
    let item = quarantine_store.item(id).await.unwrap().unwrap();
    let audit = PlatformAudit::new(PgAuditStore::new(pool), "acme");
    assert_eq!(audit.scope(), scope_of("acme"));
    let entry = AuditEntry {
        action: AuditAction::Retried,
        item: id,
        record_ref: item.record_ref.clone(),
        source: item.source.clone(),
        actor: "agent:agent-1".to_owned(),
        reason: String::new(),
        at: 1_790_000_000_000_000,
    };
    audit.record(&entry).await.unwrap();
    let head = store
        .head(&scope_of("acme"))
        .await
        .unwrap()
        .expect("an entry on the chain");
    assert_eq!(head.seq, 1);
    assert_eq!(head.record.kind, "quarantine_retried");
    assert_eq!(head.record.actor, "agent:agent-1");
    // Another site's chain is separate.
    assert!(store.head(&scope_of("globex")).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_without_a_database_provides_no_port_and_does_not_fail() {
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    ctx.as_module(quarantine::NAME, quarantine::ORDER);
    quarantine::register(&mut ctx).await.unwrap();
    assert!(ctx.port::<dyn QuarantineBackend>().is_none());
    // A reload of a site that never provided one provides none either.
    let mut reload = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        false,
    );
    reload.as_module(quarantine::NAME, quarantine::ORDER);
    quarantine::register(&mut reload).await.unwrap();
    assert!(reload.port::<dyn QuarantineBackend>().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_module_provides_nothing() {
    let mut settings = BTreeMap::new();
    let mut table = toml::map::Map::new();
    table.insert("enabled".to_owned(), toml::Value::Boolean(false));
    settings.insert(quarantine::NAME.to_owned(), toml::Value::Table(table));
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock(), settings, true);
    ctx.as_module(quarantine::NAME, quarantine::ORDER);
    quarantine::register(&mut ctx).await.unwrap();
    assert!(ctx.port::<dyn QuarantineBackend>().is_none());
}
