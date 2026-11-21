//! Shared setup: a scratch database, a bridge on a lazily built pool, the stores, and the record
//! fixtures. Tests never read a clock; every instant is an argument.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use deadpool_postgres::Pool;
use serde_json::json;
use spot_bpm_contract::{ProcessContract, contracts, from_text};
use spot_bpm_conversations::record::Intent;
use spot_bpm_conversations::{ConversationRecord, Wait, WaitFor, WaitKind};
use spot_bpm_conversations_pg::PgConversationStore;
use spot_bpm_model::{RejectAction, TimeoutAction};
use spot_link_exchange::{Conversation, MessageId};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_tenant_link::LinkId;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (printing `SKIPPED:`) when no server is configured. With
/// `SPOT_REQUIRE_PG=1` a missing server panics instead.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

static NAMES: AtomicUsize = AtomicUsize::new(0);

pub fn pool_of(config: PgConfig, size: usize) -> Pool {
    let mut config = config;
    config.pool_max_size = size;
    PgBackend::connect_unchecked(config)
        .expect("backend")
        .pool()
}

/// A bridge on its own pool of 12 (in flight 8).
pub fn open_bridge(config: PgConfig) -> (Arc<Bridge>, Pool) {
    let pool = pool_of(config, 12);
    let bridge = Arc::new(
        Bridge::new(
            BridgeConfig {
                name: format!("bpm_conv_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                workers: 2,
                max_in_flight: 8,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(20),
            },
            pool.clone(),
        )
        .expect("bridge"),
    );
    (bridge, pool)
}

/// A store for `tenant` on a fresh bridge, with its tables created.
pub fn open_store(config: PgConfig, tenant: &str) -> (Arc<Bridge>, Arc<PgConversationStore>) {
    let (bridge, pool) = open_bridge(config);
    let store =
        Arc::new(PgConversationStore::new(Arc::clone(&bridge), &pool, tenant).expect("store"));
    store.prepare().expect("prepare");
    (bridge, store)
}

/// A second store for another tenant on the same bridge.
pub fn sibling(bridge: &Arc<Bridge>, config: PgConfig, tenant: &str) -> Arc<PgConversationStore> {
    let pool = pool_of(config, 12);
    Arc::new(PgConversationStore::new(Arc::clone(bridge), &pool, tenant).expect("store"))
}

pub fn contract() -> ProcessContract {
    from_text(contracts::REQUEST_APPROVE).unwrap()
}

/// A link id made from `n`.
pub fn link(n: u8) -> LinkId {
    LinkId::new_v7(1_700_000_000_000 + u64::from(n), [n; 10])
}

/// A message id made from `n` (later `n`, later id).
pub fn mid(n: u32) -> MessageId {
    MessageId::new_v7(1_700_000_000_000 + u64::from(n), [7; 10])
}

pub fn conv(name: &str) -> Conversation {
    Conversation::new(name).unwrap()
}

/// A fresh record for `(link, name)`.
pub fn record(link: LinkId, name: &str) -> ConversationRecord {
    ConversationRecord::new(link, conv(name), &contract(), "requester", "approver")
}

/// A record that has sent a request and holds a delivery wait.
pub fn open_record(link: LinkId, name: &str, n: u32) -> ConversationRecord {
    let c = contract();
    let mut r = record(link, name);
    r.append_sent(&c, "request", mid(n), 1_000 + u64::from(n))
        .unwrap();
    r
}

/// A record that has every field filled: sent and received events with a payload, both kinds of
/// wait, intents, consumption, an attention marker and a compensation flag.
pub fn rich_record(link: LinkId, name: &str) -> ConversationRecord {
    let c = contract();
    let mut r = open_record(link, name, 1);
    r.case = Some("case-1".to_owned());
    r.intents.insert(
        "req:send:1".to_owned(),
        Intent {
            id: mid(1),
            message: "request".to_owned(),
        },
    );
    r.register_wait(Wait {
        node: "wait_approval".to_owned(),
        attempt: 2,
        kind: WaitKind::Message {
            message: "approve".to_owned(),
            on_timeout: TimeoutAction::TimeoutEdge,
        },
        due_ms: Some(9_999),
    })
    .unwrap();
    r.register_wait(Wait {
        node: "send_request".to_owned(),
        attempt: 1,
        kind: WaitKind::Delivery {
            id: mid(1),
            wait_for: WaitFor::Applied,
            on_reject: RejectAction::Continue,
        },
        due_ms: None,
    })
    .unwrap();
    r.append_received(
        &c,
        "approve",
        mid(2),
        2_000,
        json!({ "reference": "REF-1", "nested": { "a": [1, 2, 3], "b": null } }),
    )
    .unwrap();
    r.consumed.insert(mid(2));
    r.consumed_by
        .insert(("wait_approval".to_owned(), 2), mid(2));
    r.arrivals_done.insert(mid(2));
    r.mark_attention("bpm_conversations.wait_timeout", 3_000);
    r.compensated = true;
    r
}
