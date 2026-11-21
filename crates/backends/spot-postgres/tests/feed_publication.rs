//! The outbox and change feed on PostgreSQL: late commits, causal order, net changes and
//! concurrent publishers (ADR-053 "Outbox and change feed").
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::collections::BTreeSet;

use documents_support::*;
use spot_storage_contract::{
    ChangeFeed, ChangeOp, DocumentData, DocumentStore, FeedPosition, NodeRef, Outbox, OutboxEvent,
    RecordId, StorageBackend, Transaction, Version,
};

fn event(doc: &DocumentData, topic: &str) -> OutboxEvent {
    OutboxEvent {
        id: RecordId::new_v7(),
        topic: n(topic),
        subject: NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        },
        subject_version: None,
        schema_version: 1,
        payload: r#"{"ok":true}"#.into(),
    }
}

fn ids(records: &[spot_storage_contract::OutboxRecord]) -> Vec<RecordId> {
    records.iter().map(|r| r.event.id).collect()
}

#[tokio::test]
async fn a_late_commit_is_returned_by_the_next_read_and_never_skipped() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let (ea, eb) = (event(&p, "a"), event(&p, "b"));
    // A enqueues and stays open; B enqueues and commits.
    let mut a = begin(&env, &cat).await;
    a.enqueue(&ea).await.unwrap();
    let mut b = begin(&env, &cat).await;
    b.enqueue(&eb).await.unwrap();
    b.commit().await.unwrap();
    let first = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(ids(&first), vec![eb.id]);
    // A commits after the reader has seen B.
    a.commit().await.unwrap();
    let second = env
        .backend
        .read_outbox(first[0].position, 10)
        .await
        .unwrap();
    assert_eq!(
        ids(&second),
        vec![ea.id],
        "the late commit must not be skipped"
    );
    assert!(second[0].position > first[0].position);
    // And the whole log is exactly once each.
    let all = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(ids(&all), vec![eb.id, ea.id]);
}

#[tokio::test]
async fn a_late_commit_of_a_document_change_is_returned_by_the_next_read() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let (p1, p2) = (party("One", 0), party("Two", 0));
    let mut a = begin(&env, &cat).await;
    a.insert(&p1).await.unwrap();
    let mut b = begin(&env, &cat).await;
    b.insert(&p2).await.unwrap();
    b.commit().await.unwrap();
    let first = env
        .backend
        .read_changes(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].document.id, p2.id);
    a.commit().await.unwrap();
    let second = env
        .backend
        .read_changes(first[0].position, 10)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].document.id, p1.id);
}

#[tokio::test]
async fn causal_order_holds_across_three_transactions() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    // T1 is open (a lower transaction id) while T2 and T3 run and commit one after another.
    let mut t1 = begin(&env, &cat).await;
    let e1 = event(&p, "one");
    t1.enqueue(&e1).await.unwrap();
    let mut t2 = begin(&env, &cat).await;
    let e2 = event(&p, "two");
    t2.enqueue(&e2).await.unwrap();
    t2.commit().await.unwrap();
    // T3 writes after T2 committed; T1 commits last, after T3.
    let mut t3 = begin(&env, &cat).await;
    let e3 = event(&p, "three");
    t3.enqueue(&e3).await.unwrap();
    t3.commit().await.unwrap();
    t1.commit().await.unwrap();
    let all = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    let order = ids(&all);
    let at = |id: RecordId| order.iter().position(|x| *x == id).unwrap();
    assert_eq!(order.len(), 3);
    assert!(at(e2.id) < at(e3.id), "T3 was written after T2 committed");
}

#[tokio::test]
async fn an_entry_written_after_a_commit_follows_all_of_that_commits_entries() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    // A transaction that took its id early, and so has the lower one.
    let mut early = begin(&env, &cat).await;
    early.enqueue(&event(&p, "first")).await.unwrap();
    let mut other = begin(&env, &cat).await;
    let (x, y) = (event(&p, "x"), event(&p, "y"));
    other.enqueue(&x).await.unwrap();
    other.enqueue(&y).await.unwrap();
    other.commit().await.unwrap();
    // Written only now, after `other` committed, though `early` has the lower transaction id.
    let late = event(&p, "late");
    early.enqueue(&late).await.unwrap();
    early.commit().await.unwrap();
    let all = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    let order = ids(&all);
    let at = |id: RecordId| order.iter().position(|v| *v == id).unwrap();
    assert!(at(x.id) < at(y.id), "transaction order");
    assert!(at(y.id) < at(late.id), "causal order");
}

#[tokio::test]
async fn the_change_feed_nets_a_transaction_to_one_entry_per_document() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let mut created = party("Created", 0);
    let transient = party("Transient", 0);
    let mut tx = begin(&env, &cat).await;
    tx.insert(&created).await.unwrap();
    created.fields.insert(n("credit_limit"), money(500));
    tx.update(&created, Version::FIRST).await.unwrap();
    tx.insert(&transient).await.unwrap();
    let mut t2 = transient.clone();
    t2.fields.insert(n("credit_limit"), money(1));
    tx.update(&t2, Version::FIRST).await.unwrap();
    tx.delete(&transient.entity, transient.id, Version(2))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let changes = env
        .backend
        .read_changes(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(changes.len(), 1, "got {changes:?}");
    assert_eq!(changes[0].document.id, created.id);
    assert_eq!(changes[0].op, ChangeOp::Insert);
    assert_eq!(changes[0].version, Version(2));

    // A child-only change is an update of the parent; update then delete is a delete.
    let p = party("Owner", 0);
    let mut inv = invoice("I-1", p.id, vec![line("a", 1_000, 100)]);
    seed(&env, &cat, &[&p, &inv]).await;
    let head = env
        .backend
        .read_changes(FeedPosition::START, 100)
        .await
        .unwrap()
        .last()
        .unwrap()
        .position;
    let mut tx = begin(&env, &cat).await;
    inv.children
        .get_mut(&n("lines"))
        .unwrap()
        .push(line("b", 2_000, 200));
    tx.update(&inv, Version::FIRST).await.unwrap();
    tx.commit().await.unwrap();
    let next = env.backend.read_changes(head, 100).await.unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(
        (next[0].op, next[0].version),
        (ChangeOp::Update, Version(2))
    );
    assert_eq!(next[0].document.entity, n("invoice"));

    let mut tx = begin(&env, &cat).await;
    inv.fields.insert(n("total"), money(7));
    tx.update(&inv, Version(2)).await.unwrap();
    tx.delete(&inv.entity, inv.id, Version(3)).await.unwrap();
    tx.commit().await.unwrap();
    let last = env
        .backend
        .read_changes(next[0].position, 100)
        .await
        .unwrap();
    assert_eq!(last.len(), 1);
    assert_eq!(
        (last[0].op, last[0].version),
        (ChangeOp::Delete, Version(3))
    );
}

#[tokio::test]
async fn rolled_back_savepoints_leave_no_captured_entries() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let (kept, dropped) = (party("Kept", 0), party("Dropped", 0));
    let mut tx = begin(&env, &cat).await;
    tx.insert(&kept).await.unwrap();
    tx.enqueue(&event(&kept, "kept")).await.unwrap();
    let sp = tx.savepoint().await.unwrap();
    tx.insert(&dropped).await.unwrap();
    tx.enqueue(&event(&dropped, "dropped")).await.unwrap();
    tx.rollback_to(sp).await.unwrap();
    tx.enqueue(&event(&kept, "after")).await.unwrap();
    tx.commit().await.unwrap();
    let out = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(
        out.iter()
            .map(|r| r.event.topic.as_str())
            .collect::<Vec<_>>(),
        vec!["kept", "after"]
    );
    assert_eq!(
        out.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
        vec![0, 1],
        "ordinals stay dense after a savepoint rollback"
    );
    assert_eq!(out[0].transaction, out[1].transaction);
    let changes = env
        .backend
        .read_changes(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].document.id, kept.id);
}

#[tokio::test]
async fn eight_concurrent_publishers_give_no_duplicate_or_missing_positions() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let mut expected = BTreeSet::new();
    for round in 0..6 {
        let mut tx = begin(&env, &cat).await;
        for k in 0..3 {
            let e = event(&p, &format!("t{round}_{k}"));
            expected.insert(e.id);
            tx.enqueue(&e).await.unwrap();
        }
        tx.commit().await.unwrap();
    }
    // Writers keep committing while eight readers publish at once.
    let writer = {
        let backend = env.backend.clone();
        let cat = cat.clone();
        let p = p.clone();
        tokio::spawn(async move {
            let mut ids = Vec::new();
            for k in 0..10 {
                let mut tx = backend
                    .begin(
                        cat.clone(),
                        spot_storage_contract::TxOptions::new(
                            n("writer"),
                            spot_storage_contract::Isolation::ReadCommitted,
                        ),
                    )
                    .await
                    .unwrap();
                let e = event(&p, &format!("w{k}"));
                ids.push(e.id);
                tx.enqueue(&e).await.unwrap();
                tx.commit().await.unwrap();
            }
            ids
        })
    };
    let mut readers = Vec::new();
    for _ in 0..8 {
        let backend = env.backend.clone();
        readers.push(tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut at = FeedPosition::START;
            loop {
                let page = backend.read_outbox(at, 5).await.unwrap();
                if page.is_empty() {
                    break;
                }
                at = page.last().unwrap().position;
                seen.extend(page);
            }
            seen
        }));
    }
    let written = writer.await.unwrap();
    expected.extend(written);
    for reader in readers {
        let seen = reader.await.unwrap();
        let seqs: Vec<u64> = seen.iter().map(|r| r.position.seq()).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "positions increase");
        let unique: BTreeSet<_> = seen.iter().map(|r| r.event.id).collect();
        assert_eq!(unique.len(), seen.len(), "no event twice");
    }
    // After everything committed, a final read has every event exactly once, with dense positions.
    let all = env
        .backend
        .read_outbox(FeedPosition::START, 1_000)
        .await
        .unwrap();
    let got: BTreeSet<_> = all.iter().map(|r| r.event.id).collect();
    assert_eq!(got, expected);
    assert_eq!(all.len(), expected.len());
    let seqs: Vec<u64> = all.iter().map(|r| r.position.seq()).collect();
    assert_eq!(seqs, (1..=all.len() as u64).collect::<Vec<_>>());
}
