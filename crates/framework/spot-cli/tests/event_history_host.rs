//! The history reader the `event_history` module builds, over a journal written by the real
//! shipper into a folder store (WP-JRN-09.T3): 300 `doc_state` events shipped and sealed come back
//! in order with no gap and no duplicate, the seam bound is honoured, and a reader of another site
//! on the same folder root reads none of them.
//!
//! The stream engine and the HTTP routes that would call this reader are not wired by this task
//! (see its report); the L3 case therefore reads through the hosted reader of a real node.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use spot_cli::services::event_history::build_reader;
use spot_cli::services::journal::{
    FolderStore, JournalKeys, PassThrough, Ports, Shipper, ShipperConfig, Step, StoreRelayFactory,
    TenantScoped,
};
use spot_events_contract::envelope::Position;
use spot_journal_relay::{Durability, RelayConfig};
use spot_journal_segment::SealLimits;
use spot_journal_ship::OutboxEntry;
use spot_journal_ship::memory::{MemoryLease, MemoryOutbox, MemoryWatermarks, RecordingAlarms};
use spot_secrets::MemoryKeyring;

fn line(seq: u64) -> OutboxEntry {
    let text = json!({
        "event_id": format!("01900000-0000-7000-8000-{:012x}", 1_000_000 + seq),
        "topic": "doc_state", "schema_version": 1, "source_id": "acme.example",
        "position": {"generation": 1, "seq": seq},
        "transaction": format!("01900000-0000-7000-8000-{:012x}", 2_000_000 + seq),
        "ordinal": 0,
        "subject": {"entity": "project", "id": format!("01900000-0000-7000-8000-{seq:012x}")},
        "subject_version": 1,
        "meta": {"occurred_at": "2026-01-01T00:00:00Z",
                 "actor": {"kind": "system", "component": "scheduler"},
                 "correlation_id": format!("01900000-0000-7000-8000-{:012x}", 3_000_000 + seq)},
        "data": {"op": "insert", "entity_schema": 1, "image": {"id": seq, "title": "t"}},
    })
    .to_string();
    OutboxEntry {
        position: Position { generation: 1, seq },
        line: text.into_bytes(),
    }
}

/// Ships `count` events of `site` into `root` and returns the site's keys.
async fn ship(root: &std::path::Path, site: &str, count: u64) -> Arc<JournalKeys> {
    let keys = Arc::new(
        JournalKeys::derive(Arc::new(
            MemoryKeyring::new(&format!("key-{site}")).unwrap(),
        ))
        .await
        .unwrap(),
    );
    let outbox = Arc::new(MemoryOutbox::new());
    for seq in 1..=count {
        outbox.push(line(seq));
    }
    let path = root.to_path_buf();
    let tenant = site.to_owned();
    let make_store = Arc::new(move || {
        let folder =
            FolderStore::new(&path).map_err(|_| spot_journal_ship::ShipFault::StoreDown {
                code: "journal_store.io",
            })?;
        TenantScoped::new(folder, &tenant)
            .map_err(|_| spot_journal_ship::ShipFault::Config("tenant"))
    });
    let signer_keys = Arc::clone(&keys);
    let mut relay_config =
        RelayConfig::new(site, site, Durability::RemoteSync).with_tail_keys(keys.tail_keys());
    relay_config.limits = SealLimits {
        max_bytes: 1,
        max_age_millis: 0,
    };
    let factory = StoreRelayFactory::new(
        make_store,
        Arc::new(move || signer_keys.signer()),
        keys.verifying(),
        relay_config,
    );
    let mut cfg = ShipperConfig::new(site, "remote-sync", true);
    cfg.batch_max_entries = 100;
    let (mut shipper, _) = Shipper::new(
        cfg,
        Ports {
            outbox,
            marks: Arc::new(MemoryWatermarks::new()),
            lease: Arc::new(MemoryLease::held()),
            protector: Arc::new(PassThrough),
            alarms: Arc::new(RecordingAlarms::new()),
            factory,
            checker: None,
            backup: None,
        },
    )
    .unwrap();
    let mut now = 1_700_000_000_000;
    for _ in 0..50 {
        now += 1_000;
        if matches!(shipper.step(now).await, Step::Idle) {
            break;
        }
    }
    keys
}

fn reader_for(
    root: &std::path::Path,
    site: &str,
    keys: &JournalKeys,
) -> Arc<dyn spot_event_stream_journal::JournalHistory> {
    let store = TenantScoped::new(FolderStore::new(root).unwrap(), site).unwrap();
    build_reader(Arc::new(store), site, keys).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_hundred_state_events_come_back_in_order_without_a_gap_or_a_duplicate() {
    let root = tempfile::tempdir().unwrap();
    let keys = ship(root.path(), "acme", 300).await;
    let reader = reader_for(root.path(), "acme", &keys);
    let all: &spot_event_stream_journal::Selector = &|_| true;
    let mut after = Position {
        generation: 0,
        seq: 0,
    };
    let mut seen = Vec::new();
    for _ in 0..20 {
        let page = reader.read_after(after, 100, None, all).await.unwrap();
        seen.extend(page.envelopes.iter().map(|e| e.position.seq));
        after = page.next_after;
        if page.reached_outbox {
            break;
        }
    }
    assert_eq!(
        seen,
        (1..=300).collect::<Vec<u64>>(),
        "in order, no gap, no duplicate"
    );
    assert_eq!(
        reader.first_available().await.unwrap(),
        Some(Position {
            generation: 1,
            seq: 1
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_stops_at_the_seam_bound() {
    let root = tempfile::tempdir().unwrap();
    let keys = ship(root.path(), "acme", 120).await;
    let reader = reader_for(root.path(), "acme", &keys);
    let all: &spot_event_stream_journal::Selector = &|_| true;
    let bound = Position {
        generation: 1,
        seq: 50,
    };
    let page = reader
        .read_after(
            Position {
                generation: 0,
                seq: 0,
            },
            500,
            Some(bound),
            all,
        )
        .await
        .unwrap();
    assert_eq!(page.envelopes.last().map(|e| e.position.seq), Some(50));
    assert!(page.reached_outbox, "the live outbox serves from 51");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_of_another_site_on_the_same_root_reads_none_of_it() {
    let root = tempfile::tempdir().unwrap();
    let keys_a = ship(root.path(), "acme", 40).await;
    let keys_b = ship(root.path(), "globex", 10).await;
    let all: &spot_event_stream_journal::Selector = &|_| true;
    // Each site reads its own events only.
    let a = reader_for(root.path(), "acme", &keys_a)
        .read_after(
            Position {
                generation: 0,
                seq: 0,
            },
            500,
            None,
            all,
        )
        .await
        .unwrap();
    let b = reader_for(root.path(), "globex", &keys_b)
        .read_after(
            Position {
                generation: 0,
                seq: 0,
            },
            500,
            None,
            all,
        )
        .await
        .unwrap();
    assert_eq!((a.envelopes.len(), b.envelopes.len()), (40, 10));
    // A reader with globex's keys cannot verify acme's manifest, and its store refuses acme's
    // prefix altogether: it never returns an acme event.
    let crossed = reader_for(root.path(), "globex", &keys_a)
        .read_after(
            Position {
                generation: 0,
                seq: 0,
            },
            500,
            None,
            all,
        )
        .await;
    if let Ok(page) = crossed {
        assert_eq!(page.envelopes.len(), 10, "only globex's own events");
    }
}
