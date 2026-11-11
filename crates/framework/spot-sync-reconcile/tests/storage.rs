//! The reference `DigestSource` over the storage fake: a consistent-point read, and the
//! false-positive guarantee under concurrent writes.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use spot_storage_contract::{RecordId, Value};
use spot_sync_reconcile::{DigestSource, Scope, reconcile};

use support::{FakeSource, fields, insert, plan, store};

fn value(text: &str) -> Value {
    Value::Text(text.to_owned())
}

fn int(value: i64) -> Value {
    Value::Int(value)
}

/// Seeds one item, one invoice, one GL entry and one stock row.
async fn seed(
    backend: &spot_storage_fake::FakeBackend,
    catalog: &std::sync::Arc<spot_storage_contract::Catalog>,
) {
    insert(
        backend,
        catalog,
        "item",
        RecordId::new_v7(),
        fields(&[("name", value("item_a")), ("qty", int(7))]),
    )
    .await;
    insert(
        backend,
        catalog,
        "invoice",
        RecordId::new_v7(),
        fields(&[("name", value("INV-1")), ("total", int(1_000))]),
    )
    .await;
    insert(
        backend,
        catalog,
        "gl_entry",
        RecordId::new_v7(),
        fields(&[
            ("name", value("gl_1")),
            ("account", value("1100")),
            ("amount", int(500)),
        ]),
    )
    .await;
    insert(
        backend,
        catalog,
        "stock_ledger",
        RecordId::new_v7(),
        fields(&[
            ("name", value("sl_1")),
            ("item", value("item_a")),
            ("qty", int(7)),
        ]),
    )
    .await;
}

#[test]
fn the_fake_source_reads_counts_totals_gl_and_stock() {
    support::block_on(async {
        let (backend, catalog) = store().await;
        seed(&backend, &catalog).await;
        let source = FakeSource::new(backend, catalog, plan(), Scope::new("company", "C1"));
        let snapshot = source.snapshot(&Scope::new("company", "C1")).await.unwrap();
        let digest = spot_sync_reconcile::Digest::of(&snapshot);
        assert_eq!(digest.count_of(&support::name("item")), 1);
        assert_eq!(digest.count_of(&support::name("invoice")), 1);
        assert_eq!(
            digest.total_of(&support::name("invoice"), &support::name("total")),
            1_000
        );
        assert_eq!(digest.gl.get("1100"), Some(&500));
        assert_eq!(digest.stock.get("item_a"), Some(&7));
    });
}

#[test]
fn writes_after_a_snapshot_do_not_cause_false_drift() {
    support::block_on(async {
        let (owner_backend, catalog) = store().await;
        let (mirror_backend, _) = store().await;
        seed(&owner_backend, &catalog).await;
        seed(&mirror_backend, &catalog).await;
        let scope = Scope::new("company", "C1");
        let owner = FakeSource::new(
            owner_backend.clone(),
            catalog.clone(),
            plan(),
            scope.clone(),
        );
        let mirror = FakeSource::new(
            mirror_backend.clone(),
            catalog.clone(),
            plan(),
            scope.clone(),
        );

        // Both nodes read their consistent point before anything moves.
        let owner_snapshot = owner.snapshot(&scope).await.unwrap();
        let mirror_snapshot = mirror.snapshot(&scope).await.unwrap();
        assert!(
            reconcile(&owner_snapshot, &mirror_snapshot, &scope).is_clean(),
            "identical stores must reconcile cleanly"
        );

        // An in-flight write lands at the mirror after its snapshot was taken.
        insert(
            &mirror_backend,
            &catalog,
            "item",
            RecordId::new_v7(),
            fields(&[("name", value("item_late")), ("qty", int(1))]),
        )
        .await;

        // The snapshots already taken are unaffected: no false drift.
        assert!(
            reconcile(&owner_snapshot, &mirror_snapshot, &scope).is_clean(),
            "a write after the snapshot must not change it"
        );

        // A fresh read sees it, so the clean result above was not a bug.
        let fresh = mirror.snapshot(&scope).await.unwrap();
        let report = reconcile(&owner_snapshot, &fresh, &scope);
        assert_eq!(report.of_kind("extra").len(), 1, "{:?}", report.findings);
    });
}

#[test]
fn concurrent_writes_keep_every_snapshot_consistent() {
    support::block_on(async {
        let (backend, catalog) = store().await;
        seed(&backend, &catalog).await;
        let scope = Scope::new("company", "C1");
        let source = FakeSource::new(backend.clone(), catalog.clone(), plan(), scope.clone());

        // A writer inserts one hundred items while the reader snapshots the scope.
        let writer_backend = backend.clone();
        let writer_catalog = catalog.clone();
        let writer = tokio::spawn(async move {
            for i in 0..100 {
                insert(
                    &writer_backend,
                    &writer_catalog,
                    "item",
                    RecordId::new_v7(),
                    fields(&[("name", value(&format!("item_{i:03}"))), ("qty", int(1))]),
                )
                .await;
            }
        });

        let mut last_bookmark = None;
        let mut snapshots = 0;
        for _ in 0..50 {
            let snapshot = source.snapshot(&scope).await.unwrap();
            // A snapshot is a consistent point: no key appears twice.
            let mut keys = BTreeSet::new();
            for facts in &snapshot.records {
                assert!(
                    keys.insert((facts.key.entity.clone(), facts.key.name.clone())),
                    "a snapshot held the same record twice"
                );
            }
            if let Some(previous) = last_bookmark {
                assert!(
                    snapshot.bookmark >= previous,
                    "bookmarks never go backwards"
                );
            }
            last_bookmark = Some(snapshot.bookmark);
            snapshots += 1;
            tokio::task::yield_now().await;
        }
        writer.await.unwrap();
        assert_eq!(snapshots, 50);

        // Once the writer is done, a fresh read holds everything it wrote.
        let final_snapshot = source.snapshot(&scope).await.unwrap();
        let items = final_snapshot
            .records
            .iter()
            .filter(|facts| facts.key.entity == support::name("item"))
            .count();
        assert_eq!(items, 101, "one seeded item plus one hundred written");
    });
}

#[test]
fn a_missing_document_in_storage_is_detected_in_one_cycle() {
    support::block_on(async {
        let (owner_backend, catalog) = store().await;
        let (mirror_backend, _) = store().await;
        seed(&owner_backend, &catalog).await;
        // The mirror gets everything but the invoice.
        insert(
            &mirror_backend,
            &catalog,
            "item",
            RecordId::new_v7(),
            fields(&[("name", value("item_a")), ("qty", int(7))]),
        )
        .await;
        let scope = Scope::new("company", "C1");
        let owner = FakeSource::new(owner_backend, catalog.clone(), plan(), scope.clone());
        let mirror = FakeSource::new(mirror_backend, catalog, plan(), scope.clone());

        let report = spot_sync_reconcile::reconcile_sources(&owner, &mirror, &scope)
            .await
            .unwrap();
        assert!(
            report
                .of_kind("missing")
                .iter()
                .any(|finding| matches!(finding, spot_sync_reconcile::Finding::MissingAtReceiver { name, .. } if name == "INV-1")),
            "{:?}",
            report.findings
        );
    });
}
