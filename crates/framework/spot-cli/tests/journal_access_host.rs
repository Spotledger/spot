//! The journal access surface hosted over a real PostgreSQL database: grants persist with their
//! `sys_journal_grant` events in one transaction, and every call of the port, denied ones
//! included, writes exactly one `sys_journal_access` outbox event.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use serde_json::Value as Json;
use spot_cli::services::journal_access::{Callers, port};
use spot_core::SiteId;
use spot_http::api::files::next_chunk;
use spot_http::api::ports::journal_access::{JournalAccessError, JournalAccessPort};
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_journal_access::pg::{DDL, PgAccessAudit, PgGrantStore};
use spot_journal_access::{
    Access, AdminDirectory, DAY_MILLIS, Fut, GrantStore, IdSource, JournalMeta, JournalReader,
    Limits, ReaderParts, Settings, TimeSource,
};
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys, append_segments, sign};
use spot_journal_store::{JournalStore, Layout, MemoryStore, etag};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation, RecordId, StorageBackend};

const T0: i64 = 1_800_000_000_000;

struct Env {
    db: TestDb,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    // The host never creates the table (`spot migrate` does, from `schema::extra_tables`).
    db.raw_client().await.batch_execute(DDL).await.unwrap();
    Some(Env {
        db,
        backend,
        catalog,
    })
}

#[derive(Default)]
struct Admins(Mutex<HashSet<String>>);
impl AdminDirectory for Admins {
    fn is_tenant_administrator<'a>(&'a self, principal: &'a str) -> Fut<'a, bool> {
        Box::pin(async move { Ok(self.0.lock().unwrap().contains(principal)) })
    }
}

struct NewIds;
impl IdSource for NewIds {
    fn new_id(&self) -> String {
        RecordId::new_v7().to_string()
    }
}

struct Clock(AtomicI64);
impl TimeSource for Clock {
    fn now_millis(&self) -> Option<i64> {
        Some(self.0.load(Ordering::SeqCst))
    }
}

/// Tokens are the user ids themselves; an unknown token is unauthenticated.
struct TokenCallers(HashSet<String>);
impl Callers for TokenCallers {
    fn caller<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<String, JournalAccessError>> {
        Box::pin(async move {
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default();
            if self.0.contains(&token) {
                Ok(token)
            } else {
                Err(JournalAccessError::Unauthenticated)
            }
        })
    }
}

fn ctx(token: &str) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("acme").unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("req-1").unwrap(),
        peer: None,
    }
}

async fn events(env: &Env, topic: &str) -> Vec<Json> {
    let client = env.db.raw_client().await;
    client
        .query(
            "SELECT payload FROM spot_outbox WHERE topic = $1 ORDER BY captured_at, ordinal",
            &[&topic],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| serde_json::from_str(&row.get::<_, String>(0)).unwrap())
        .collect()
}

fn segment_bytes() -> Vec<u8> {
    (0..70_000usize).map(|i| (i % 251) as u8).collect()
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let hex = etag(bytes);
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

async fn journal() -> (Arc<MemoryStore>, VerifyingKeys, Vec<u8>) {
    let store = Arc::new(MemoryStore::new());
    let signing = SigningKey::from_bytes(&[9u8; 32]);
    let mut keys = VerifyingKeys::new();
    keys.add_key("k1", signing.verifying_key(), 1, 10).unwrap();
    let bytes = segment_bytes();
    let manifest = append_segments(
        &Manifest::first("acme", T0).unwrap(),
        &[SegmentSummary::new(
            1,
            1,
            100,
            100,
            hash_bytes(&bytes),
            [0u8; 32],
        )],
    )
    .unwrap();
    store
        .put_new(
            &Layout::manifest("acme", 1).unwrap().to_key(),
            sign(&manifest, &signing, "k1").to_bytes(),
        )
        .await
        .unwrap();
    store
        .put_new(
            &Layout::segment("acme", 1, 1, 100).unwrap().to_key(),
            bytes.clone(),
        )
        .await
        .unwrap();
    (store, keys, bytes)
}

struct Hosted {
    access: Arc<Access>,
    admins: Arc<Admins>,
    clock: Arc<Clock>,
    port: Arc<dyn JournalAccessPort>,
    admin: String,
    auditor: String,
    stranger: String,
    bytes: Vec<u8>,
}

async fn host(env: &Env) -> Hosted {
    let grants = Arc::new(PgGrantStore::new(
        env.backend.clone(),
        Arc::clone(&env.catalog),
    ));
    grants.check().await.unwrap();
    let audit = Arc::new(PgAccessAudit::new(
        env.backend.clone(),
        Arc::clone(&env.catalog),
    ));
    let admins = Arc::new(Admins::default());
    let (admin, auditor, stranger) = (
        RecordId::new_v7().to_string(),
        RecordId::new_v7().to_string(),
        RecordId::new_v7().to_string(),
    );
    admins.0.lock().unwrap().insert(admin.clone());
    let access = Arc::new(
        Access::new(
            grants as Arc<dyn GrantStore>,
            admins.clone(),
            Arc::new(NewIds),
            Settings::default(),
        )
        .unwrap(),
    );
    let (store, keys, bytes) = journal().await;
    let clock = Arc::new(Clock(AtomicI64::new(T0)));
    let reader = JournalReader::new(ReaderParts {
        tenant: "acme".to_owned(),
        store,
        keys: Arc::new(keys),
        access: access.clone(),
        audit,
        limits: Arc::new(Limits::new(4, 1 << 30)),
        clock: clock.clone(),
        meta: JournalMeta {
            policy: "standard".to_owned(),
            region: "eu".to_owned(),
        },
    })
    .unwrap();
    let callers = TokenCallers(
        [admin.clone(), auditor.clone(), stranger.clone()]
            .into_iter()
            .collect(),
    );
    Hosted {
        access,
        admins,
        clock,
        port: port(Arc::new(callers), reader),
        admin,
        auditor,
        stranger,
        bytes,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_persist_with_their_events_in_one_transaction() {
    let Some(env) = env().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let h = host(&env).await;
    let granted = h
        .access
        .grant(&h.admin, &h.auditor, true, None, T0)
        .await
        .unwrap();
    h.access
        .renew(&h.admin, &granted.id, 14, T0 + 1)
        .await
        .unwrap();
    // A journal_reader cannot grant: refused, and the refusal is recorded.
    assert!(
        h.access
            .grant(&h.stranger, &h.admin, false, None, T0)
            .await
            .is_err()
    );
    h.access
        .revoke(&h.admin, &granted.id, T0 + 2)
        .await
        .unwrap();
    let actions: Vec<String> = events(&env, "sys_journal_grant")
        .await
        .iter()
        .map(|e| e["action"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(actions, ["granted", "renewed", "refused", "revoked"]);
    // The row says what the events say.
    let rows = h.access.list(&h.admin).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].revoked_by.as_deref(), Some(h.admin.as_str()));
    // No second live grant while one is live; a revoked one does not block a new grant.
    assert!(
        h.access
            .grant(&h.admin, &h.auditor, true, Some(3), T0 + 3)
            .await
            .is_ok()
    );
    // Nothing of a credential or content in any event.
    for event in events(&env, "sys_journal_grant").await {
        assert!(event.get("token").is_none() && event.get("secret").is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_call_writes_exactly_one_access_record_denied_ones_included() {
    let Some(env) = env().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let h = host(&env).await;
    let mut expected = 0usize;
    // An ungranted user, a bad path, an unlisted segment: refused, one record each.
    assert_eq!(
        h.port.info(&ctx(&h.stranger)).await.unwrap_err(),
        JournalAccessError::Denied { expired: false }
    );
    expected += 1;
    h.access
        .grant(&h.admin, &h.auditor, true, None, T0)
        .await
        .unwrap();
    for (generation, range) in [("1", "1-99"), ("..", "1-100"), ("2", "1-100")] {
        let refused = h
            .port
            .segment(&ctx(&h.auditor), generation, range, None)
            .await;
        assert_eq!(refused.err(), Some(JournalAccessError::NotFound));
        expected += 1;
    }
    // A granted auditor reads the manifest and the segment; the bytes hash to the manifest.
    let manifest = h.port.manifest(&ctx(&h.auditor), "1").await.unwrap();
    assert_eq!(manifest["generation"], 1);
    expected += 1;
    let mut download = h
        .port
        .segment(&ctx(&h.auditor), "1", "1-100", None)
        .await
        .unwrap();
    let mut body = download.body.take().unwrap();
    let mut got = Vec::new();
    while let Some(chunk) = next_chunk(&mut body).await {
        got.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(got, h.bytes);
    assert_eq!(download.etag, etag(&h.bytes));
    expected += 1;
    // The administrator reads through the same audited path, as `admin`.
    h.port.info(&ctx(&h.admin)).await.unwrap();
    expected += 1;
    // An unknown credential is unauthenticated and names no principal: no record.
    assert_eq!(
        h.port.info(&ctx("nobody")).await.unwrap_err(),
        JournalAccessError::Unauthenticated
    );
    let records = events(&env, "sys_journal_access").await;
    assert_eq!(records.len(), expected, "{records:?}");
    let outcomes: Vec<&str> = records
        .iter()
        .map(|r| r["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        [
            "denied",
            "not_found",
            "not_found",
            "not_found",
            "served",
            "served",
            "served"
        ]
    );
    assert_eq!(records[5]["bytes"], h.bytes.len());
    assert_eq!(records[6]["grant"], "admin");
    // Never content: the payload names the object and the range only.
    assert!(records.iter().all(|r| r.get("content").is_none()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_and_revocation_end_access_at_the_next_request() {
    let Some(env) = env().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let h = host(&env).await;
    let granted = h
        .access
        .grant(&h.admin, &h.auditor, true, Some(1), T0)
        .await
        .unwrap();
    h.port.info(&ctx(&h.auditor)).await.unwrap();
    h.clock.0.store(T0 + 2 * DAY_MILLIS, Ordering::SeqCst);
    assert_eq!(
        h.port.info(&ctx(&h.auditor)).await.unwrap_err(),
        JournalAccessError::Denied { expired: true }
    );
    h.access
        .renew(&h.admin, &granted.id, 2, T0 + 2 * DAY_MILLIS)
        .await
        .unwrap();
    h.port.info(&ctx(&h.auditor)).await.unwrap();
    h.access
        .revoke(&h.admin, &granted.id, T0 + 2 * DAY_MILLIS + 1)
        .await
        .unwrap();
    assert_eq!(
        h.port.info(&ctx(&h.auditor)).await.unwrap_err(),
        JournalAccessError::Denied { expired: false }
    );
    // The grantor losing the administrator role voids a fresh grant.
    h.access
        .grant(&h.admin, &h.auditor, true, None, T0 + 2 * DAY_MILLIS + 2)
        .await
        .unwrap();
    h.clock
        .0
        .store(T0 + 2 * DAY_MILLIS + 60_000, Ordering::SeqCst);
    h.port.info(&ctx(&h.auditor)).await.unwrap();
    h.admins.0.lock().unwrap().remove(&h.admin);
    h.clock
        .0
        .store(T0 + 2 * DAY_MILLIS + 120_000, Ordering::SeqCst);
    assert_eq!(
        h.port.info(&ctx(&h.auditor)).await.unwrap_err(),
        JournalAccessError::Denied { expired: false }
    );
}
