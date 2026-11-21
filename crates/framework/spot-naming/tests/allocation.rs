//! Allocation on the storage `Sequences`: concurrency, rollback, blocks, tickets and admin.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_naming::{GapPolicy, Namer, NamingError};
use spot_storage_contract::Transaction;
use time::Month;

fn fixed_ctx() -> spot_naming::NamingContext {
    ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0))
}

async fn number(namer: &Namer, doctype: &str, tx: &mut spot_storage_fake::FakeTx) -> u64 {
    namer
        .ticket(
            &name(doctype),
            &doc(doctype),
            &fixed_ctx(),
            GapPolicy::Tolerant,
            tx,
        )
        .await
        .unwrap()
        .number()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_transactions_allocate_one_and_two() {
    let backend = backend();
    let namer = Arc::new(namer());

    let task = tokio::spawn({
        let backend = backend.clone();
        let namer = Arc::clone(&namer);
        async move {
            let mut tx = begin(&backend).await;
            let number = namer
                .ticket(
                    &name(SERIES_DOCTYPE),
                    &doc(SERIES_DOCTYPE),
                    &fixed_ctx(),
                    GapPolicy::Tolerant,
                    &mut tx,
                )
                .await
                .unwrap()
                .number()
                .unwrap();
            tx.commit().await.unwrap();
            number
        }
    });

    // Give the spawned transaction time to reach the series lock, then allocate here too.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut tx = begin(&backend).await;
    let local = number(&namer, SERIES_DOCTYPE, &mut tx).await;
    tx.commit().await.unwrap();
    let remote = task.await.unwrap();

    let mut got = [local, remote];
    got.sort_unstable();
    assert_eq!(got, [1, 2], "the two transactions never share a number");
}

#[tokio::test]
async fn a_rolled_back_allocation_returns_its_number() {
    let backend = backend();
    let namer = namer();

    let mut tx = begin(&backend).await;
    let first = number(&namer, SERIES_DOCTYPE, &mut tx).await;
    assert_eq!(first, 1);
    tx.rollback().await.unwrap();

    let mut tx = begin(&backend).await;
    let again = number(&namer, SERIES_DOCTYPE, &mut tx).await;
    assert_eq!(again, 1, "a rolled-back number is handed out again");
}

#[tokio::test]
async fn a_reserved_block_of_ten_reports_three_left_after_seven() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;

    let mut block = namer
        .reserve(&doctype, &fixed_ctx(), GapPolicy::Reserved(10), &mut tx)
        .await
        .unwrap();
    assert_eq!(block.total(), 10);
    assert_eq!(block.remaining(), 10);

    let mut consumed = Vec::new();
    for _ in 0..7 {
        consumed.push(block.next_number().unwrap());
    }
    assert_eq!(consumed, (1..=7).collect::<Vec<_>>());
    assert_eq!(block.remaining(), 3);

    for _ in 0..3 {
        block.next_number().unwrap();
    }
    assert!(matches!(
        block.next_number().unwrap_err(),
        NamingError::BlockExhausted { count: 10 }
    ));
}

#[tokio::test]
async fn a_block_hands_out_names() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let mut block = namer
        .reserve(&doctype, &fixed_ctx(), GapPolicy::Reserved(2), &mut tx)
        .await
        .unwrap();
    assert_eq!(block.next_name().unwrap().as_str(), "INV-2026-00001");
    assert_eq!(block.next_name().unwrap().as_str(), "INV-2026-00002");
}

#[tokio::test]
async fn iterator_and_remaining_agree() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let block = namer
        .reserve(&doctype, &fixed_ctx(), GapPolicy::Reserved(5), &mut tx)
        .await
        .unwrap();
    assert_eq!(block.clone().collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    assert_eq!(block.remaining(), 5);
}

#[tokio::test]
async fn set_next_is_forward_only() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let key = namer.series_key(&doctype, &fixed_ctx()).unwrap();

    namer.set_next(&key, 5, &mut tx).await.unwrap();
    assert_eq!(number(&namer, SERIES_DOCTYPE, &mut tx).await, 5);

    let err = namer.set_next(&key, 3, &mut tx).await.unwrap_err();
    assert!(matches!(err, NamingError::SeriesBackwards { .. }));
    assert_eq!(err.code(), "naming.series_backwards");
}

#[tokio::test]
async fn peek_does_not_consume() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let key = namer.series_key(&doctype, &fixed_ctx()).unwrap();
    namer.set_next(&key, 42, &mut tx).await.unwrap();

    assert_eq!(namer.peek(&key, &mut tx).await.unwrap(), 42);
    assert_eq!(
        namer.peek(&key, &mut tx).await.unwrap(),
        42,
        "peek is read-only"
    );
    assert_eq!(number(&namer, SERIES_DOCTYPE, &mut tx).await, 42);
}

#[tokio::test]
async fn strict_refuses_skip_and_reserve() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let key = namer.series_key(&doctype, &fixed_ctx()).unwrap();

    let skip = namer
        .skip(GapPolicy::GaplessStrict, &key, 1, &mut tx)
        .await
        .unwrap_err();
    assert_eq!(skip.code(), "naming.skip_refused");

    let reserve = namer
        .reserve(&doctype, &fixed_ctx(), GapPolicy::GaplessStrict, &mut tx)
        .await
        .unwrap_err();
    assert_eq!(reserve.code(), "naming.reserve_refused");
}

#[tokio::test]
async fn a_tolerant_series_may_skip() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let key = namer.series_key(&doctype, &fixed_ctx()).unwrap();

    namer
        .skip(GapPolicy::Tolerant, &key, 2, &mut tx)
        .await
        .unwrap();
    assert_eq!(number(&namer, SERIES_DOCTYPE, &mut tx).await, 3);
}

#[tokio::test]
async fn a_strict_ticket_commits_the_name() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let ticket = namer
        .ticket(
            &doctype,
            &doc(SERIES_DOCTYPE),
            &fixed_ctx(),
            GapPolicy::GaplessStrict,
            &mut tx,
        )
        .await
        .unwrap();
    assert_eq!(ticket.policy(), GapPolicy::GaplessStrict);
    assert_eq!(ticket.number(), Some(1));
    assert_eq!(ticket.name().as_str(), "INV-2026-00001");
    assert_eq!(ticket.commit_name().as_str(), "INV-2026-00001");
}

#[tokio::test]
async fn a_reserved_policy_needs_reserve_and_a_tolerant_one_does_not() {
    let backend = backend();
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;

    let err = namer
        .name_for(
            &doctype,
            &doc(SERIES_DOCTYPE),
            &fixed_ctx(),
            GapPolicy::Reserved(3),
            &mut tx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "naming.policy_unsupported");

    let err = namer
        .reserve(&doctype, &fixed_ctx(), GapPolicy::Tolerant, &mut tx)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "naming.policy_unsupported");
}
