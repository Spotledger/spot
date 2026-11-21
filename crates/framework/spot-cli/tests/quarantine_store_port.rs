//! The quarantine module's `QuarantineStore` port (WP-AI-07.T17): the module provides the
//! library's store, the same instance the backend is built over, beside the backend port, so the
//! edge intake's quarantine adapter can be wired. An item put through the store port is listed by
//! the backend and the other way round; `provide_store = false`, a disabled module and a site
//! without a database provide no store; a reload provides the first build's store; a second
//! provider of the port is a startup error naming both modules.
//!
//! The PostgreSQL tests skip (printing a reason) without `SPOT_TEST_PG_URL`; the others need no
//! database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_cli::services::SiteCtx;
use spot_cli::services::controllers::platform_permissions;
use spot_cli::services::quarantine::live::{self, SitePrincipals, backend_over};
use spot_cli::services::quarantine::{
    self, BackendError, Decision, Item, QuarantineBackend, State, provide_ports,
};
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
    InMemoryStore, NewItem, QuarantineAudit, QuarantineStore, RecordRef, RecordingAudit,
};
use spot_quarantine_pg::PgQuarantineStore;
use spot_replica_apply::ReceivedRecord;
use spot_storage_contract::RecordId;
use spot_sync_protocol::{RecordName, SourceId};
use time::OffsetDateTime;
use tokio_postgres::NoTls;

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
    ))
}

fn ctx(label: &str, assembling: bool) -> SiteCtx<'static> {
    let mut ctx = SiteCtx::new(
        label,
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        assembling,
    );
    ctx.as_module(quarantine::NAME, quarantine::ORDER);
    ctx
}

fn ctx_with(label: &str, assembling: bool, key: &str, value: bool) -> SiteCtx<'static> {
    let mut table = toml::map::Map::new();
    table.insert(key.to_owned(), toml::Value::Boolean(value));
    let mut settings = BTreeMap::new();
    settings.insert(quarantine::NAME.to_owned(), toml::Value::Table(table));
    let mut ctx = SiteCtx::new(label, Duration::from_secs(1), clock(), settings, assembling);
    ctx.as_module(quarantine::NAME, quarantine::ORDER);
    ctx
}

/// A backend that answers nothing: the tests that need no database only count ports.
struct Stub;

#[async_trait]
impl QuarantineBackend for Stub {
    async fn list(&self, _: &str, _: Option<&str>, _: usize) -> Result<Vec<Item>, BackendError> {
        Err(BackendError::Unavailable)
    }
    async fn get(&self, _: &str, _: &str) -> Result<Option<Item>, BackendError> {
        Err(BackendError::Unavailable)
    }
    async fn image(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
        Err(BackendError::Unavailable)
    }
    async fn transition(
        &self,
        _: &str,
        _: &str,
        _: State,
        _: Decision,
        _: &str,
        _: Option<&str>,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unavailable)
    }
    async fn summary(&self, _: &str) -> Result<Vec<Item>, BackendError> {
        Err(BackendError::Unavailable)
    }
}

fn stub() -> Arc<dyn QuarantineBackend> {
    Arc::new(Stub)
}

fn memory() -> Arc<dyn QuarantineStore> {
    Arc::new(InMemoryStore::new())
}

#[test]
fn both_ports_are_provided_together() {
    let mut ctx = ctx("store-port-both", true);
    provide_ports(&mut ctx, stub(), Some(memory())).unwrap();
    assert!(ctx.port::<dyn QuarantineBackend>().is_some());
    assert!(ctx.port::<dyn QuarantineStore>().is_some());
}

#[test]
fn a_backend_only_build_provides_no_store() {
    let mut ctx = ctx("store-port-backend-only", true);
    provide_ports(&mut ctx, stub(), None).unwrap();
    assert!(ctx.port::<dyn QuarantineBackend>().is_some());
    assert!(ctx.port::<dyn QuarantineStore>().is_none());
}

#[test]
fn a_second_provider_of_the_store_is_a_startup_error_naming_both_modules() {
    let mut ctx = ctx("store-port-twice", true);
    ctx.as_module("another_module", 1300);
    ctx.provide::<dyn QuarantineStore>(memory()).unwrap();
    ctx.as_module(quarantine::NAME, quarantine::ORDER);
    let refused = provide_ports(&mut ctx, stub(), Some(memory())).unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("another_module"), "{text}");
    assert!(text.contains(quarantine::NAME), "{text}");
    assert!(text.contains("QuarantineStore"), "{text}");
    // The other direction: the store is ours, a later module cannot provide it again.
    let mut ours = ctx_with("store-port-twice-2", true, "enabled", true);
    provide_ports(&mut ours, stub(), Some(memory())).unwrap();
    ours.as_module("another_module", 1500);
    assert!(ours.provide::<dyn QuarantineStore>(memory()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_module_and_a_site_without_a_database_provide_no_store() {
    // Disabled: nothing, even with `provide_store` left at its default.
    let mut disabled = ctx_with("store-port-disabled", true, "enabled", false);
    quarantine::register(&mut disabled).await.unwrap();
    assert!(disabled.port::<dyn QuarantineBackend>().is_none());
    assert!(disabled.port::<dyn QuarantineStore>().is_none());
    // No database: the context carries no site, so nothing is provided and nothing fails.
    let mut bare = ctx("store-port-no-db", true);
    quarantine::register(&mut bare).await.unwrap();
    assert!(bare.port::<dyn QuarantineBackend>().is_none());
    assert!(bare.port::<dyn QuarantineStore>().is_none());
    // A reload of a site that never provided either provides neither.
    let mut reload = ctx("store-port-no-db", false);
    quarantine::register(&mut reload).await.unwrap();
    assert!(reload.port::<dyn QuarantineBackend>().is_none());
    assert!(reload.port::<dyn QuarantineStore>().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_provides_the_first_builds_store_and_provide_store_false_withholds_it() {
    let site = "store-port-reload";
    let first = memory();
    live::remember(site, stub());
    live::remember_store(site, Arc::clone(&first));

    let mut reload = ctx(site, false);
    quarantine::register(&mut reload).await.unwrap();
    assert!(reload.port::<dyn QuarantineBackend>().is_some());
    let again = reload
        .port::<dyn QuarantineStore>()
        .expect("a reload provides the store");
    assert!(
        Arc::ptr_eq(&first, &again),
        "the reload provides the first build's instance, not a second store"
    );

    let mut backend_only = ctx_with(site, false, "provide_store", false);
    quarantine::register(&mut backend_only).await.unwrap();
    assert!(backend_only.port::<dyn QuarantineBackend>().is_some());
    assert!(backend_only.port::<dyn QuarantineStore>().is_none());

    // Disabling the module forgets both, so a later reload provides nothing.
    let mut disabled = ctx_with(site, false, "enabled", false);
    quarantine::register(&mut disabled).await.unwrap();
    assert!(live::recall(site).is_none());
    assert!(live::recall_store(site).is_none());
    let mut after = ctx(site, false);
    quarantine::register(&mut after).await.unwrap();
    assert!(after.port::<dyn QuarantineStore>().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_setting_is_refused() {
    let mut ctx = ctx_with("store-port-typo", true, "provide_stor", true);
    assert!(quarantine::register(&mut ctx).await.is_err());
}

// ---- PostgreSQL: the two ports are one store ----

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
    let name = AppPermissionName::parse("sync_quarantine_discard").unwrap();
    catalogue.insert(name.clone(), decl).unwrap();
    let grants = vec![AppPermissionGrant {
        role: Role("Reviewer".to_owned()),
        permission: name,
    }];
    let snapshot = Snapshot::build_with_apps(&meta, &model, &catalogue, &grants).unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

struct Site;

impl SitePrincipalSource for Site {
    fn resolve<'a>(&'a self, user: &'a RecordId) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            Principal::user(
                &user.to_string(),
                vec![AssignmentContext::unscoped("a1", &["Reviewer"])],
            )
        })
    }
}

fn record(seq: u64) -> ReceivedRecord {
    let id = RecordId::new_v7();
    let payload = serde_json::json!({
        "meta_version": 1,
        "document": {
            "entity": "note",
            "id": id.to_string(),
            "fields": { "title": { "t": "text", "v": "hello" } }
        }
    })
    .to_string();
    ReceivedRecord::from_payload(
        &SourceId::new("edge-1#b1").unwrap(),
        RecordName::new(&format!("note:{id}")).unwrap(),
        1,
        seq,
        &payload,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_store_port_and_the_backend_are_one_store() {
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let pool = pool(&db);
    let pg = Arc::new(PgQuarantineStore::new(pool));
    pg.prepare().await.unwrap();
    let shared: Arc<dyn QuarantineStore> = pg;
    let audit = Arc::new(RecordingAudit::new());
    let backend = backend_over(
        "acme",
        Arc::clone(&shared),
        Arc::clone(&audit) as Arc<dyn QuarantineAudit>,
        engine(),
        Arc::new(SitePrincipals::new(Arc::new(Site))),
        clock(),
    );
    let mut ctx = ctx("store-port-pg", true);
    provide_ports(&mut ctx, backend, Some(shared)).unwrap();
    let store = ctx.port::<dyn QuarantineStore>().expect("the store port");
    let backend = ctx
        .port::<dyn QuarantineBackend>()
        .expect("the backend port");

    // An item put through the store port is listed by the backend.
    let rec = record(1);
    let new = NewItem::held(
        rec.source.clone(),
        rec.seq,
        RecordRef::of(&rec),
        "replica_apply.invalid",
        "the shape was refused",
        1_000_000,
    );
    let id = store.insert(new, rec).await.unwrap().id;
    let listed = backend.list("acme", None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id.0.to_string());
    assert_eq!(listed[0].state, State::Held);

    // A decision taken through the backend is seen through the store port.
    let actor = format!("agent:{}", RecordId::new_v7());
    backend
        .transition(
            "acme",
            &id.0.to_string(),
            State::Retrying,
            Decision::Retried,
            &actor,
            None,
        )
        .await
        .unwrap();
    let item = store.item(id).await.unwrap().unwrap();
    assert_eq!(item.state, spot_quarantine::State::Retrying);
    assert_eq!(audit.entries().len(), 1, "the decision was audited once");
}
