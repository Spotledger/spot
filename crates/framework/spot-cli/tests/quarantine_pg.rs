//! The quarantine admin over the library's PostgreSQL store (WP-RTB-03.T9): discard closes with
//! who and when, only through the real permission engine; a denied discard changes and audits
//! nothing; a failed audit changes nothing; tenants are separate; the condition follows the
//! held items. Skips (printing a reason) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_cli::services::controllers::platform_permissions;
use spot_cli::services::quarantine::backend::StoreBackend;
use spot_cli::services::quarantine::condition::{ConditionPort, QuarantinedSync};
use spot_cli::services::quarantine::guards::{EngineCloseGuard, PrincipalSource};
use spot_cli::services::quarantine::{
    BackendError, GuardedAdmin, QuarantineBackend, ReadScope, Refusal, State,
};
use spot_core::{Clock, ManualClock};
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
use spot_replica_apply::ReceivedRecord;
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

struct Principals;

impl PrincipalSource for Principals {
    fn principal(&self, actor: &str) -> Option<Principal> {
        match actor {
            "ada" => Some(Principal::user(
                "ada",
                vec![AssignmentContext::unscoped("a1", &["Reviewer"])],
            )),
            "nobody" => Some(Principal::user("nobody", vec![])),
            _ => None,
        }
    }
}

struct OpenScope;

impl ReadScope for OpenScope {
    fn may_read(&self, _actor: &str, _doctype: &str) -> bool {
        true
    }

    fn may_read_field(&self, _actor: &str, _doctype: &str, _field: &str) -> bool {
        true
    }
}

fn record(entity: &str, seq: u64) -> ReceivedRecord {
    let id = spot_storage_contract::RecordId::new_v7();
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

struct Fixture {
    _db: TestDb,
    store: Arc<PgQuarantineStore>,
    audit: Arc<Audit>,
    backend: StoreBackend,
}

async fn fixture() -> Option<Fixture> {
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let store = Arc::new(PgQuarantineStore::new(pool(&db)));
    store.prepare().await.unwrap();
    let audit = Arc::new(Audit {
        inner: RecordingAudit::new(),
        fail: Mutex::new(false),
    });
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
    ));
    let backend = StoreBackend::new(
        "acme",
        Arc::clone(&store) as Arc<dyn QuarantineStore>,
        Arc::clone(&audit) as Arc<dyn QuarantineAudit>,
        engine(),
        Arc::new(Principals),
        clock,
    );
    Some(Fixture {
        _db: db,
        store,
        audit,
        backend,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discard_closes_with_who_and_when_and_is_audited() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let admin = GuardedAdmin::new(
        f.backend,
        EngineCloseGuard::new(engine(), Arc::new(Principals)),
        OpenScope,
        "acme",
    );
    admin
        .discard("ada", &id.0.to_string(), "duplicate")
        .await
        .unwrap();
    let item = f.store.item(id).await.unwrap().unwrap();
    assert_eq!(item.state, spot_quarantine::State::Discarded);
    let closure = f
        .store
        .closure(id)
        .await
        .unwrap()
        .expect("a closure record");
    assert_eq!(closure.by, "ada");
    let entries = f.audit.inner.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, AuditAction::Discarded);
    assert_eq!(entries[0].reason, "duplicate");
    // Closed is terminal.
    assert_eq!(
        admin
            .discard("ada", &id.0.to_string(), "again")
            .await
            .unwrap_err(),
        Refusal::WrongState
    );
    assert_eq!(f.audit.inner.entries().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_discard_changes_and_audits_nothing() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let admin = GuardedAdmin::new(
        f.backend,
        EngineCloseGuard::new(engine(), Arc::new(Principals)),
        OpenScope,
        "acme",
    );
    // No role: the permission is not granted. An unknown actor: no principal, fail closed.
    for actor in ["nobody", "stranger"] {
        assert_eq!(
            admin
                .discard(actor, &id.0.to_string(), "x")
                .await
                .unwrap_err(),
            Refusal::Forbidden
        );
    }
    let item = f.store.item(id).await.unwrap().unwrap();
    assert_eq!(item.state, spot_quarantine::State::Held);
    assert!(f.audit.inner.entries().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_audit_leaves_the_item_untouched() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    *f.audit.fail.lock().unwrap() = true;
    let admin = GuardedAdmin::new(
        f.backend,
        EngineCloseGuard::new(engine(), Arc::new(Principals)),
        OpenScope,
        "acme",
    );
    assert_eq!(
        admin
            .discard("ada", &id.0.to_string(), "dup")
            .await
            .unwrap_err(),
        Refusal::Unavailable
    );
    assert_eq!(
        admin.retry("ada", &id.0.to_string()).await.unwrap_err(),
        Refusal::Unavailable
    );
    let item = f.store.item(id).await.unwrap().unwrap();
    assert_eq!(
        item.state,
        spot_quarantine::State::Held,
        "no decision, no effect"
    );
    assert!(f.store.closure(id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_moves_the_item_and_another_tenant_sees_nothing() {
    let Some(f) = fixture().await else { return };
    let id = put(&f.store, 1).await;
    let id_text = id.0.to_string();
    f.backend
        .transition(
            "acme",
            &id_text,
            State::Retrying,
            spot_cli::services::quarantine::Decision::Retried,
            "ada",
            None,
        )
        .await
        .unwrap();
    let item = f.store.item(id).await.unwrap().unwrap();
    assert_eq!(item.state, spot_quarantine::State::Retrying);
    assert_eq!(f.audit.inner.entries()[0].action, AuditAction::Retried);
    // Another tenant's name: nothing listed, nothing found, nothing changed.
    assert!(f.backend.list("globex", None, 10).await.unwrap().is_empty());
    assert!(f.backend.get("globex", &id_text).await.unwrap().is_none());
    assert_eq!(
        f.backend
            .transition(
                "globex",
                &id_text,
                State::Discarded,
                spot_cli::services::quarantine::Decision::Discarded,
                "ada",
                Some("x")
            )
            .await
            .unwrap_err(),
        BackendError::Conflict
    );
    // The held image is its document fields; a missing id has none.
    let image = f.backend.image("acme", &id_text).await.unwrap().unwrap();
    assert!(image.contains("title"));
    assert!(f.backend.image("acme", "999999").await.unwrap().is_none());
}

#[derive(Default)]
struct Port(Mutex<BTreeSet<String>>);

#[async_trait]
impl ConditionPort for Port {
    async fn raise(
        &self,
        subject: &str,
        _node: &str,
        _record: &str,
        _cause: &str,
    ) -> Result<(), String> {
        self.0.lock().unwrap().insert(subject.to_owned());
        Ok(())
    }

    async fn clear(&self, subject: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(subject);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_condition_follows_the_held_items() {
    let Some(f) = fixture().await else { return };
    let a = put(&f.store, 1).await;
    let _b = put(&f.store, 2).await;
    let port = Port::default();
    let sync = QuarantinedSync::new();
    let summary = f.backend.summary("acme").await.unwrap();
    let first = sync.reconcile(&summary, &port).await;
    assert_eq!(first.raised.len(), 2);
    let admin = GuardedAdmin::new(
        f.backend,
        EngineCloseGuard::new(engine(), Arc::new(Principals)),
        OpenScope,
        "acme",
    );
    admin.discard("ada", &a.0.to_string(), "dup").await.unwrap();
    let summary = admin.backend().summary("acme").await.unwrap();
    let after = sync.reconcile(&summary, &port).await;
    assert_eq!(after.cleared, vec![a.0.to_string()]);
    assert_eq!(port.0.lock().unwrap().len(), 1);
    assert!(
        admin
            .readiness()
            .await
            .starts_with("quarantine: 1 items (oldest ")
    );
}
