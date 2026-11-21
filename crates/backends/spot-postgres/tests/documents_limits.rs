//! Write limits, read-only transactions, savepoints and volume for documents. The outbox halves
//! of the contract's `savepoint_rollback` and `write_limit_is_enforced` cases arrive with
//! WP-STO-08.T1; their document halves are here.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use documents_support::*;
use spot_storage_contract::{
    DocumentStore, ErrorKind, Isolation, StorageBackend, Transaction, TxOptions, Version,
};

#[tokio::test]
async fn read_only_transactions_reject_document_writes() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    seed(&env, &cat, &[&p]).await;
    let options = TxOptions::new(n("report"), Isolation::RepeatableRead).read_only();
    let mut tx = env.backend.begin(cat.clone(), options).await.unwrap();
    expect_kind(
        tx.insert(&party("B", 0)).await,
        ErrorKind::ReadOnlyTransaction,
    );
    // Reading is fine in the same kind of transaction.
    let options = TxOptions::new(n("report"), Isolation::RepeatableRead).read_only();
    let mut tx = env.backend.begin(cat.clone(), options).await.unwrap();
    let doc = tx
        .load(&n("party"), p.id, spot_storage_contract::RowLock::None)
        .await
        .unwrap();
    assert!(doc.is_some());
}

#[tokio::test]
async fn child_rows_count_against_max_writes() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("A", 0);
    // 1 invoice + 3 lines = 4 records against a limit of 3.
    let inv = invoice(
        "I",
        p.id,
        vec![
            line("a", 1_000, 1),
            line("b", 1_000, 1),
            line("c", 1_000, 1),
        ],
    );
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 3).await;
    expect_kind(tx.insert(&inv).await, ErrorKind::TransactionTooLarge);
    tx.rollback().await.unwrap();
    // The refusal came before any SQL: nothing of the document exists afterwards.
    assert!(load(&env, &cat, "invoice", inv.id).await.is_none());

    let ok = invoice("J", p.id, vec![line("a", 1_000, 1), line("b", 1_000, 1)]);
    // 1 (party) + 3 (invoice with two lines) = 4 fits exactly; one more record does not.
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 4).await;
    tx.insert(&p).await.unwrap();
    tx.insert(&ok).await.unwrap();
    expect_kind(
        tx.insert(&party("Over", 0)).await,
        ErrorKind::TransactionTooLarge,
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn updates_and_deletes_count_every_child_row_they_touch() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("A", 0);
    let mut inv = invoice(
        "I",
        p.id,
        vec![
            line("a", 1_000, 1),
            line("b", 1_000, 1),
            line("c", 1_000, 1),
        ],
    );
    seed(&env, &cat, &[&p, &inv]).await;

    // Updating touches the parent and all three kept lines: 4 writes.
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 3).await;
    expect_kind(
        tx.update(&inv, Version::FIRST).await,
        ErrorKind::TransactionTooLarge,
    );
    tx.rollback().await.unwrap();
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 4).await;
    tx.update(&inv, Version::FIRST).await.unwrap();
    tx.commit().await.unwrap();

    // Dropping two lines and adding one: parent + 1 kept + 2 deleted + 1 inserted = 5.
    let kept = lines_of(&inv)[0].clone();
    inv.children
        .insert(n("lines"), vec![kept, line("d", 1_000, 1)]);
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 4).await;
    expect_kind(
        tx.update(&inv, Version(2)).await,
        ErrorKind::TransactionTooLarge,
    );
    tx.rollback().await.unwrap();
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 5).await;
    tx.update(&inv, Version(2)).await.unwrap();
    tx.commit().await.unwrap();

    // Deleting counts the document and its two remaining lines.
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 2).await;
    expect_kind(
        tx.delete(&n("invoice"), inv.id, Version(3)).await,
        ErrorKind::TransactionTooLarge,
    );
    tx.rollback().await.unwrap();
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 3).await;
    tx.delete(&n("invoice"), inv.id, Version(3)).await.unwrap();
    tx.commit().await.unwrap();
    assert!(load(&env, &cat, "invoice", inv.id).await.is_none());
}

#[tokio::test]
async fn writes_undone_by_a_savepoint_are_uncounted_and_unstored() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let (keep, undo) = (party("Keep", 0), party("Undo", 0));
    let mut tx = begin_with(&env, &cat, |o| o.max_writes = 2).await;
    tx.insert(&keep).await.unwrap();
    let sp = tx.savepoint().await.unwrap();
    tx.insert(&undo).await.unwrap();
    tx.rollback_to(sp).await.unwrap();
    // A failed insert inside a savepoint leaves the transaction usable after rollback_to.
    let mut dup = party("Dup", 0);
    dup.id = keep.id;
    expect_kind(tx.insert(&dup).await, ErrorKind::AlreadyExists);
    tx.rollback_to(sp).await.unwrap();
    // The undone insert no longer counts: one more fits in the limit of two.
    tx.insert(&party("Later", 0)).await.unwrap();
    tx.release(sp).await.unwrap();
    tx.commit().await.unwrap();
    assert!(load(&env, &cat, "party", keep.id).await.is_some());
    assert!(load(&env, &cat, "party", undo.id).await.is_none());
}

#[tokio::test]
async fn a_thousand_documents_with_ten_rows_each_round_trip_unchanged() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Everyone", 0);
    seed(&env, &cat, &[&p]).await;
    let mut docs = Vec::new();
    for i in 0..1_000 {
        let lines = (0..10)
            .map(|j| line(&format!("item {i}.{j}"), 1_000 + j, 100 * (i + 1) + j))
            .collect();
        docs.push(invoice(&format!("INV-{i:04}"), p.id, lines));
    }
    // 500 documents of 11 records each stay below the backend's limit of 10,000 per transaction.
    for chunk in docs.chunks(500) {
        let mut tx = begin_with(&env, &cat, |o| o.max_writes = 5_500).await;
        for doc in chunk {
            tx.insert(doc).await.unwrap();
        }
        tx.commit().await.unwrap();
    }
    let mut tx = begin(&env, &cat).await;
    for doc in &docs {
        let stored = tx
            .load(&doc.entity, doc.id, spot_storage_contract::RowLock::None)
            .await
            .unwrap()
            .expect("stored");
        assert_eq!(&stored.data, doc, "document {:?}", doc.fields[&n("number")]);
        assert_eq!(stored.version, Version::FIRST);
        assert_eq!(lines_of(&stored.data).len(), 10);
        assert_eq!(
            item_of(&lines_of(&stored.data)[9]),
            item_of(&lines_of(doc)[9])
        );
    }
    tx.rollback().await.unwrap();
}
