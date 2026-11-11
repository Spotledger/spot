//! The secret audit journal in the audit chain (WP-SEC-03.T7, F1): a `sys_secret_audit` outbox
//! event is chained exactly once, in order with document audit events, survives an appender
//! restart, is tamper-evident like a document entry, and can carry no value.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use serde_json::json;
use spot_app_api::context::Actor;
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId};
use spot_audit_chain::{
    AppendOutcome, AuditStore, ChainEntry, Checkpoint, CheckpointSigner, InMemoryStore, KeyRing,
    LocalSigner, PayloadValue, StoreFuture,
};
use spot_core::ManualClock;
use spot_doc_audit::{
    AppenderStore, DocAudit, DocAuditError, DocOperation, InMemoryAppenderStore, LaneAppender,
    LaneConfig, OutboxFeed, SECRET_AUDIT_TOPIC, SECRET_EVENT_KEYS, decode_secret_outbox,
    enqueue_audit, is_secret_entry, lane_site, secret_event_of,
};
use spot_events_contract::Topic;
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{
    Catalog, Isolation, NodeRef, OutboxEvent, StorageBackend, Transaction, TxOptions,
    contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "test.spotledger.app";
/// A sentinel that must never appear in a chain entry: no secret event holds it.
const SENTINEL: &str = "tok_live_8f3a9c1e5d7b2046";

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> <FakeBackend as StorageBackend>::Tx {
    b.begin(
        cat.clone(),
        TxOptions::new(Name::new("secret_chain").unwrap(), Isolation::ReadCommitted),
    )
    .await
    .unwrap()
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([7u8; 32]))
}

fn keys() -> KeyRing {
    let local = LocalSigner::from_seed([7u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    keys
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH))
}

/// A secret audit outbox event shaped like the engine's (`engine/outbox.rs`), about the record
/// `n` of the fixture DocType, with `data` as its payload data.
fn secret_event(n: u64, data: serde_json::Value) -> OutboxEvent {
    let id = RecordId::new_v7();
    let meta = EventMeta {
        occurred_at: OffsetDateTime::UNIX_EPOCH
            + time::Duration::seconds(i64::try_from(n).unwrap()),
        actor: Actor::System {
            component: Name::new("login_service").unwrap(),
        },
        command_id: None,
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    PayloadDocument { meta, data }
        .to_outbox_event(
            id,
            &Topic::any(SECRET_AUDIT_TOPIC).unwrap(),
            NodeRef {
                entity: Name::new(DOCTYPE).unwrap(),
                id: RecordId::from_bytes(id_bytes(n)).unwrap(),
            },
            None,
            1,
        )
        .unwrap()
}

fn id_bytes(n: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    b[6] = 0x70;
    b[8] = 0x80 | (b[8] & 0x3f);
    b
}

fn secret_data(op: &str, record: Option<&str>) -> serde_json::Value {
    let mut data = json!({
        "op": op,
        "doctype": DOCTYPE,
        "field": "token",
        "caller": "login_service",
        "site": SITE,
        "outcome": "ok",
        "state": "<set>",
    });
    if let Some(record) = record {
        data["record"] = json!(record);
    }
    data
}

async fn enqueue_secret(tx: &mut impl spot_storage_contract::Outbox, event: &OutboxEvent) {
    tx.enqueue(event).await.unwrap();
}

struct Rig {
    store: Arc<InMemoryStore>,
    state: Arc<dyn AppenderStore>,
    appender: LaneAppender<InMemoryStore>,
}

fn rig() -> Rig {
    let store = Arc::new(InMemoryStore::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        LaneConfig::new(1, 1),
        signer(),
        Arc::clone(&state),
    );
    Rig {
        store,
        state,
        appender,
    }
}

async fn entries(store: &InMemoryStore) -> Vec<ChainEntry> {
    store.range(&lane_site(SITE, 0), 1, u64::MAX).await.unwrap()
}

fn kinds(entries: &[ChainEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.record.kind.as_str()).collect()
}

/// A mixed outbox: doc, secret, doc, secret, in this order.
async fn mixed_outbox(b: &FakeBackend, cat: &Arc<Catalog>) -> Vec<OutboxEvent> {
    let mut tx = begin(b, cat).await;
    let doc1 = event(&id_of(1), 1, DocOperation::Create, "note");
    enqueue_audit(&mut tx, &doc1).await.unwrap();
    let s1 = secret_event(1, secret_data("set", Some(&id_of(1))));
    enqueue_secret(&mut tx, &s1).await;
    let doc2 = event(&id_of(1), 2, DocOperation::Update, "note");
    enqueue_audit(&mut tx, &doc2).await.unwrap();
    let s2 = secret_event(2, secret_data("open", None));
    enqueue_secret(&mut tx, &s2).await;
    tx.commit().await.unwrap();
    vec![s1, s2]
}

/// The secret events are chained once, in sequence with the document events, and a later drain
/// (or a restarted appender over the same state) does not chain them again.
#[tokio::test]
async fn a_secret_event_is_chained_once_in_sequence_with_document_events() {
    let (b, cat) = backend().await;
    mixed_outbox(&b, &cat).await;
    let r = rig();
    let feed: &dyn OutboxFeed = &b;

    let report = r.appender.drain(feed, 100).await.unwrap();
    assert_eq!((report.read, report.appended), (4, 4));
    let chained = entries(&r.store).await;
    assert_eq!(
        kinds(&chained),
        ["doc.create", "secret.set", "doc.update", "secret.open"],
        "outbox order, one lane"
    );
    assert_eq!(
        chained.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    // The entry names its topic; the two kinds are told apart.
    let secret = &chained[1];
    assert!(is_secret_entry(secret) && !is_secret_entry(&chained[0]));
    assert_eq!(
        secret.record.payload.get("topic"),
        Some(&PayloadValue::Text(SECRET_AUDIT_TOPIC.to_owned()))
    );
    let decoded = secret_event_of(secret).unwrap();
    assert_eq!(
        (
            decoded.op.as_str(),
            decoded.field.as_str(),
            decoded.state.as_str()
        ),
        ("set", "token", "<set>")
    );
    assert_eq!(decoded.actor.actor, "system");
    assert_eq!(secret.record.actor, "system");
    assert!(secret.record.masked);

    // A second drain finds nothing new.
    let again = r.appender.drain(feed, 100).await.unwrap();
    assert_eq!(again.appended, 0);

    // The restart: the bookmark is lost, the same rows are read again, nothing is chained twice.
    r.state.set_bookmark(0, FeedPosition::START);
    let restarted = LaneAppender::new(
        Arc::clone(&r.store),
        SITE,
        0,
        LaneConfig::new(1, 1),
        signer(),
        Arc::clone(&r.state),
    );
    let report = restarted.drain(feed, 100).await.unwrap();
    assert_eq!(report.appended, 0, "every row was already chained");
    assert_eq!(entries(&r.store).await.len(), 4);

    // A secret event written after the restart is chained after, once.
    let mut tx = begin(&b, &cat).await;
    enqueue_secret(&mut tx, &secret_event(3, secret_data("find", None))).await;
    tx.commit().await.unwrap();
    let report = restarted.drain(feed, 100).await.unwrap();
    assert_eq!(report.appended, 1);
    let chained = entries(&r.store).await;
    assert_eq!(chained.len(), 5);
    assert_eq!(chained[4].record.kind, "secret.find");
}

/// A tampered secret entry is detected as a tampered document entry is, and the head after it
/// no longer links.
#[tokio::test]
async fn a_tampered_secret_entry_is_detected_like_a_document_entry() {
    let (b, cat) = backend().await;
    mixed_outbox(&b, &cat).await;
    let store = Arc::new(Tamper::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let signer = signer();
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        LaneConfig::new(1, 1),
        Arc::clone(&signer),
        Arc::clone(&state),
    );
    let feed: &dyn OutboxFeed = &b;
    appender.drain(feed, 100).await.unwrap();
    let service = DocAudit::new(
        Arc::clone(&store),
        SITE,
        LaneConfig::new(1, 1),
        Arc::clone(&signer),
        keys(),
        masker(),
        Arc::clone(&state),
        clock(),
    );
    let clean = service.verify_site(None).await.unwrap();
    assert!(clean.is_ok(), "{:?}", clean.failures);
    assert_eq!(clean.checked, 4);

    // Entry 2 is the secret entry.
    store.tamper(&lane_site(SITE, 0), 2);
    let report = service.verify_site(None).await.unwrap();
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.lane == Some(0) && f.seq == 2),
        "reports the secret entry: {:?}",
        report.failures
    );
}

/// A chain append that fails does not drop the outbox row: the bookmark stays, and the next drain
/// chains it, once.
#[tokio::test]
async fn a_failed_chain_append_does_not_drop_the_outbox_row() {
    let (b, cat) = backend().await;
    let mut tx = begin(&b, &cat).await;
    enqueue_secret(&mut tx, &secret_event(1, secret_data("set", None))).await;
    tx.commit().await.unwrap();

    let store = Arc::new(Flaky::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        LaneConfig::new(1, 1),
        signer(),
        Arc::clone(&state),
    );
    let feed: &dyn OutboxFeed = &b;
    store.fail.store(true, Ordering::SeqCst);
    let failed = appender.drain(feed, 100).await;
    assert!(matches!(failed, Err(DocAuditError::Chain(_))));
    assert_eq!(
        state.bookmark(0),
        FeedPosition::START,
        "the row was not passed"
    );
    assert!(
        store
            .inner
            .range(&lane_site(SITE, 0), 1, 9)
            .await
            .unwrap()
            .is_empty()
    );

    store.fail.store(false, Ordering::SeqCst);
    let report = appender.drain(feed, 100).await.unwrap();
    assert_eq!(report.appended, 1);
    let chained = store.inner.range(&lane_site(SITE, 0), 1, 9).await.unwrap();
    assert_eq!(chained.len(), 1);
    assert_eq!(chained[0].record.kind, "secret.set");
}

/// No value can reach the chain: an event with any key beyond the engine's, a non-text value or an
/// unknown outcome is refused and not chained, and the lane waits on it rather than passing it.
#[tokio::test]
async fn an_event_that_could_carry_a_value_is_not_chained() {
    let bad_datas = [
        {
            let mut d = secret_data("set", None);
            d["value"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["token"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["field"] = json!({"nested": SENTINEL});
            d
        },
        {
            let mut d = secret_data("set", None);
            d["op"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["caller"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["record"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["state"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["outcome"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["error"] = json!(SENTINEL);
            d
        },
        {
            let mut d = secret_data("set", None);
            d["doctype"] = json!(SENTINEL.repeat(40));
            d
        },
    ];
    for data in bad_datas {
        let (b, cat) = backend().await;
        let mut tx = begin(&b, &cat).await;
        enqueue_secret(&mut tx, &secret_event(1, data.clone())).await;
        tx.commit().await.unwrap();
        let r = rig();
        let feed: &dyn OutboxFeed = &b;
        let result = r.appender.drain(feed, 100).await;
        let Err(DocAuditError::Encoding(message)) = result else {
            panic!("expected an encoding refusal for {data}");
        };
        assert!(
            !message.contains(SENTINEL),
            "the refusal names no content: {message}"
        );
        assert!(entries(&r.store).await.is_empty(), "nothing was chained");
        assert_eq!(r.state.bookmark(0), FeedPosition::START, "the row stays");
    }
}

/// A DocType, field or site that is not name-shaped (a refused call may carry any text) is
/// chained as `<invalid>`: the lane is not stopped and the text does not reach the chain.
#[tokio::test]
async fn a_name_that_is_not_name_shaped_is_not_chained() {
    let (b, cat) = backend().await;
    let mut tx = begin(&b, &cat).await;
    let mut odd = secret_data("find", None);
    odd["doctype"] = json!(format!("my doctype {SENTINEL}!"));
    odd["field"] = json!("pass word");
    odd["outcome"] = json!("refused");
    odd["error"] = json!("meta.unknown_doctype");
    odd["state"] = json!("<none>");
    enqueue_secret(&mut tx, &secret_event(1, odd)).await;
    tx.commit().await.unwrap();
    let r = rig();
    let feed: &dyn OutboxFeed = &b;
    assert_eq!(r.appender.drain(feed, 100).await.unwrap().appended, 1);
    let chained = entries(&r.store).await;
    let event = secret_event_of(&chained[0]).unwrap();
    assert_eq!(
        (event.doctype.as_str(), event.field.as_str()),
        ("<invalid>", "<invalid>")
    );
    assert!(!format!("{:?}", chained[0]).contains(SENTINEL));
}

/// The chain entries of a realistic mix hold only the allow-listed keys, as text, and no value
/// the operations handled.
#[tokio::test]
async fn the_chain_entries_hold_only_the_journal_fields() {
    let (b, cat) = backend().await;
    let mut tx = begin(&b, &cat).await;
    // The same shapes the engine writes: ok, refused (with a code), a whole-batch rewrap.
    let mut refused = secret_data("find", None);
    refused["outcome"] = json!("refused");
    refused["error"] = json!("secretfield.not_lookup");
    refused["state"] = json!("<none>");
    let mut rewrap = secret_data("rewrap", None);
    rewrap["field"] = json!("*");
    for (n, data) in [
        secret_data("set", Some(&id_of(1))),
        refused,
        rewrap,
        secret_data("is_set", Some(&id_of(2))),
    ]
    .into_iter()
    .enumerate()
    {
        enqueue_secret(&mut tx, &secret_event(n as u64 + 1, data)).await;
    }
    tx.commit().await.unwrap();
    let r = rig();
    let feed: &dyn OutboxFeed = &b;
    assert_eq!(r.appender.drain(feed, 100).await.unwrap().appended, 4);

    let chained = entries(&r.store).await;
    assert_eq!(
        kinds(&chained),
        [
            "secret.set",
            "secret.find",
            "secret.rewrap",
            "secret.is_set"
        ]
    );
    for entry in &chained {
        assert_eq!(
            entry
                .record
                .payload
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["digest", "secret_event", "topic"]
        );
        let event = secret_event_of(entry).unwrap();
        // The event JSON holds the journal's keys and the chain's own, nothing else.
        let json = serde_json::to_value(&event).unwrap();
        let allowed: HashSet<&str> = SECRET_EVENT_KEYS
            .iter()
            .copied()
            .chain(["actor", "at", "subject_doctype", "subject_id"])
            .collect();
        for key in json.as_object().unwrap().keys() {
            assert!(allowed.contains(key.as_str()), "unexpected `{key}`");
        }
        let text = format!("{entry:?}");
        assert!(!text.contains(SENTINEL));
    }
    // The decode of an outbox row alone gives the same event the chain holds.
    let records = feed.read(FeedPosition::START, 100).await.unwrap();
    let decoded = decode_secret_outbox(&records[1]).unwrap();
    assert_eq!(decoded, secret_event_of(&chained[1]).unwrap());
    assert_eq!(decoded.error.as_deref(), Some("secretfield.not_lookup"));
}

/// The lag view counts a secret event that has not been chained, so the retention rule (a row is
/// kept until its appender has chained it) is visible, and it falls to zero once chained.
#[tokio::test]
async fn the_lag_counts_unchained_secret_events() {
    let (b, cat) = backend().await;
    mixed_outbox(&b, &cat).await;
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let store = Arc::new(InMemoryStore::new());
    let service = DocAudit::new(
        Arc::clone(&store),
        SITE,
        LaneConfig::new(1, 1),
        signer(),
        keys(),
        masker(),
        Arc::clone(&state),
        clock(),
    );
    let feed: &dyn OutboxFeed = &b;
    let lag = service.audit_lag(feed, 100).await.unwrap();
    assert_eq!(lag.pending, 4, "two document events and two secret events");
    service.appender(0).drain(feed, 100).await.unwrap();
    let lag = service.audit_lag(feed, 100).await.unwrap();
    assert!(lag.is_caught_up());
}

// ----------------------------------------------------------------- stores

/// A store whose appends can be made to fail.
struct Flaky {
    inner: InMemoryStore,
    fail: AtomicBool,
}

impl Flaky {
    fn new() -> Self {
        Self {
            inner: InMemoryStore::new(),
            fail: AtomicBool::new(false),
        }
    }
}

impl AuditStore for Flaky {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.inner.head(site)
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        if self.fail.load(Ordering::SeqCst) {
            return Box::pin(async { Err(spot_audit_chain::AuditError::RecordInvalid("down")) });
        }
        self.inner.append_if_head(site, expected_seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        self.inner.range(site, from, to)
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}

/// A store whose entries read back altered, for the verifier.
struct Tamper {
    inner: InMemoryStore,
    tampered: Mutex<HashSet<(String, u64)>>,
}

impl Tamper {
    fn new() -> Self {
        Self {
            inner: InMemoryStore::new(),
            tampered: Mutex::new(HashSet::new()),
        }
    }

    fn tamper(&self, site: &str, seq: u64) {
        self.tampered.lock().unwrap().insert((site.to_owned(), seq));
    }

    fn visible(&self, site: &str, entries: Vec<ChainEntry>) -> Vec<ChainEntry> {
        let tampered = self.tampered.lock().unwrap();
        entries
            .into_iter()
            .map(|mut e| {
                if tampered.contains(&(site.to_owned(), e.seq)) {
                    // Alter a field of the record: its stored hash no longer matches.
                    e.record.at = e.record.at.saturating_add(1);
                }
                e
            })
            .collect()
    }
}

impl AuditStore for Tamper {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            let entries = self.inner.range(site, 1, u64::MAX).await?;
            Ok(self.visible(site, entries).into_iter().last())
        })
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.inner.append_if_head(site, expected_seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let entries = self.inner.range(site, from, to).await?;
            Ok(self.visible(site, entries))
        })
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}
