//! The site views: per-document history masking and the lag/status readiness view.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::prelude::{Name, OffsetDateTime};
use spot_audit_chain::{CheckpointSigner, InMemoryStore, KeyRing, LocalSigner};
use spot_core::ManualClock;
use spot_doc_audit::{
    AppenderStore, AuditPolicy, AuditSource, DiffValue, DocAudit, DocOperation, EventSizePolicy,
    InMemoryAppenderStore, LaneAppender, LaneConfig, build_event, enqueue_audit, lane_site,
};
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "test.spotledger.app";

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> <FakeBackend as StorageBackend>::Tx {
    b.begin(
        cat.clone(),
        TxOptions::new(
            Name::new("docaudit_service").unwrap(),
            Isolation::ReadCommitted,
        ),
    )
    .await
    .unwrap()
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([9u8; 32]))
}

fn keys() -> KeyRing {
    let local = LocalSigner::from_seed([9u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    keys
}

fn clock_at(secs: i64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(secs).unwrap(),
    ))
}

fn service(
    store: Arc<InMemoryStore>,
    config: LaneConfig,
    state: Arc<dyn AppenderStore>,
    at: i64,
) -> DocAudit<InMemoryStore> {
    DocAudit::new(
        store,
        SITE,
        config,
        signer(),
        keys(),
        masker(),
        state,
        clock_at(at),
    )
}

/// The history shows the masked form to a reader without reveal and is empty for a principal who
/// cannot read the document.
#[tokio::test]
async fn history_masks_and_hides() {
    let store = Arc::new(InMemoryStore::new());
    let config = LaneConfig::new(1, 1);
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let svc = service(
        Arc::clone(&store),
        config,
        Arc::clone(&state),
        1_700_000_100,
    );

    let id = id_of(1);
    let after = image(&id, 1, "hello", "4111111111111111");
    let event = build_event(
        &*masker(),
        SITE,
        DocOperation::Create,
        &after,
        None,
        user_actor("alice"),
        1_700_000_000_000_000,
        None,
        AuditSource::Api,
        &EventSizePolicy::default(),
    )
    .unwrap();
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state),
    );
    appender.append_event("e1", &event).await.unwrap();

    let masked = svc
        .history(&principal("masked"), DOCTYPE, &id)
        .await
        .unwrap();
    assert_eq!(masked.len(), 1);
    let secret = masked[0].changes.iter().find(|c| c.path == SECRET).unwrap();
    assert_eq!(secret.after, Some(DiffValue::Text("***".to_owned())));

    let hidden = svc
        .history(&principal("nobody"), DOCTYPE, &id)
        .await
        .unwrap();
    assert!(
        hidden.is_empty(),
        "a principal who cannot read gets no history"
    );

    let reader = svc
        .history(&principal("reader"), DOCTYPE, &id)
        .await
        .unwrap();
    assert_eq!(reader.len(), 1, "a reader sees the entry");
}

/// An entry is marked verified after a successful verification run.
#[tokio::test]
async fn history_marks_verified_after_verification() {
    let store = Arc::new(InMemoryStore::new());
    let config = LaneConfig::new(1, 1);
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let svc = service(
        Arc::clone(&store),
        config,
        Arc::clone(&state),
        1_700_000_100,
    );
    let id = id_of(1);
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state),
    );
    appender
        .append_event("e1", &event(&id, 1, DocOperation::Create, "note"))
        .await
        .unwrap();
    let before = svc
        .history(&principal("masked"), DOCTYPE, &id)
        .await
        .unwrap();
    assert!(!before[0].verified);
    let report = svc.verify_site(None).await.unwrap();
    assert!(report.is_ok());
    let after = svc
        .history(&principal("masked"), DOCTYPE, &id)
        .await
        .unwrap();
    assert!(after[0].verified, "the entry is within the verified range");
}

/// Lag reports the pending events before the appender drains them, and zero after.
#[tokio::test]
async fn lag_reports_pending_events() {
    let (b, cat) = backend().await;
    let config = LaneConfig::new(2, 1);
    for i in 1..=4u64 {
        let mut tx = begin(&b, &cat).await;
        enqueue_audit(&mut tx, &event(&id_of(i), 1, DocOperation::Create, "note"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let store = Arc::new(InMemoryStore::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let svc = service(
        Arc::clone(&store),
        config,
        Arc::clone(&state),
        1_700_000_100,
    );

    let lag = svc.audit_lag(&b, 100).await.unwrap();
    assert_eq!(lag.pending, 4);
    assert_eq!(lag.oldest_age_micros, Some(100_000_000));
    let status = svc.status(&b, 100).await.unwrap();
    assert!(!status.healthy, "100s exceeds the default 60s bound");
    assert!(status.posting_allowed);

    for lane in 0..2 {
        let appender = LaneAppender::new(
            Arc::clone(&store),
            SITE,
            lane,
            config,
            signer(),
            Arc::clone(&state),
        );
        appender.drain(&b, 100).await.unwrap();
    }
    let lag = svc.audit_lag(&b, 100).await.unwrap();
    assert!(lag.is_caught_up(), "after a drain nothing is pending");
}

/// Exceeding the lag bound fails readiness; the policy flag decides whether posting stops.
#[tokio::test]
async fn lag_bound_fails_readiness() {
    let (b, cat) = backend().await;
    let config = LaneConfig::new(1, 1);
    let mut tx = begin(&b, &cat).await;
    enqueue_audit(&mut tx, &event(&id_of(1), 1, DocOperation::Create, "note"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let store = Arc::new(InMemoryStore::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    // Now is 200s after the event; the bound is 60s.
    let svc = service(
        Arc::clone(&store),
        config,
        Arc::clone(&state),
        1_700_000_200,
    );
    let warn = svc.status(&b, 100).await.unwrap();
    assert!(!warn.healthy);
    assert!(warn.posting_allowed, "the default is warn only");

    let halt = svc
        .with_policy(AuditPolicy {
            max_lag_micros: 60_000_000,
            halt_on_exceeded: true,
        })
        .status(&b, 100)
        .await
        .unwrap();
    assert!(!halt.healthy);
    assert!(!halt.posting_allowed, "with halt_on_exceeded posting stops");
}

/// A lane's document index points at the chain entries the history reads.
#[tokio::test]
async fn history_reads_the_chained_entries() {
    let store = Arc::new(InMemoryStore::new());
    let config = LaneConfig::new(1, 1);
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let svc = service(
        Arc::clone(&store),
        config,
        Arc::clone(&state),
        1_700_000_100,
    );
    let id = id_of(7);
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state),
    );
    appender
        .append_event("e1", &event(&id, 1, DocOperation::Create, "one"))
        .await
        .unwrap();
    appender
        .append_event("e2", &event(&id, 2, DocOperation::Update, "two"))
        .await
        .unwrap();
    let history = svc
        .history(&principal("reader"), DOCTYPE, &id)
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert!(history[0].seq < history[1].seq, "in append order");
    assert_eq!(history[0].version, 1);
    assert_eq!(history[1].version, 2);
    let _ = lane_site(SITE, 0);
}
