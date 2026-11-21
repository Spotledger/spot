//! Kill tests: the notice cannot be separated from the inbox write, restarts lose nothing, notice
//! numbers stay gapless and ordered under concurrency, and a large load keeps every
//! conversation in order.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use spot_link_exchange::{InboxCommit, InboxEntry, InboxStore, NoticeDraft, NoticeFeed};
use spot_link_exchange_pg::{PgExchange, apply_commit};

fn plan(w: &World, conversation_name: &str, from: u64, count: u64) -> InboxCommit {
    let mut commit = InboxCommit::new(w.link_a.id, conversation(conversation_name), from);
    commit.next_seq = from + count;
    for seq in from..from + count {
        let entry: InboxEntry = w.entry(w.message(conversation_name, seq, T0), T0);
        commit.notices.push(NoticeDraft {
            link: w.link_a.id,
            topic: entry.message.topic.clone(),
            message_id: entry.message.id,
            conversation: entry.message.conversation.clone(),
            source: w.a.clone(),
            received_at: T0,
        });
        commit.store.push(entry);
    }
    commit
}

fn nothing_persisted(e: &Env, ex: &PgExchange) {
    for table in [
        "spot_link_inbox",
        "spot_link_seen",
        "spot_link_notice",
        "spot_link_parked",
    ] {
        assert_eq!(e.count(2, table), 0, "{table} must be empty");
    }
    assert!(ex.notices().read_from(0, 10).unwrap().is_empty());
}

#[test]
fn a_failed_commit_leaves_neither_the_message_nor_its_notice() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    let inbox = ex.inbox();
    let commit = plan(&w, "orders", 1, 3);

    // The whole plan, notices included, is written; then the transaction fails.
    rt.block_on(async {
        let mut conn = inbox.connection().await.unwrap();
        let tx = inbox.begin(&mut conn).await.unwrap();
        apply_commit(&tx, &commit).await.unwrap();
        // Inside the transaction the rows exist...
        let inside: i64 = tx
            .query_one(
                "SELECT (SELECT count(*) FROM spot_link_inbox) + (SELECT count(*) FROM spot_link_notice)",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(inside, 6);
        // ...and the commit fails.
        tx.rollback().await.unwrap();
    });
    nothing_persisted(&e, &ex);
    assert_eq!(
        inbox
            .conversation(&w.link_a.id, &conversation("orders"))
            .unwrap()
            .next_seq,
        1
    );

    // The rolled-back notice numbers are handed out again: no gap.
    inbox.commit(plan(&w, "orders", 1, 3)).unwrap();
    let cursors: Vec<_> = ex
        .notices()
        .read_from(0, 10)
        .unwrap()
        .iter()
        .map(|n| n.cursor)
        .collect();
    assert_eq!(cursors, vec![1, 2, 3]);
}

#[test]
fn a_backend_killed_before_commit_leaves_nothing_behind() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    let inbox = ex.inbox();
    let commit = plan(&w, "orders", 1, 2);

    rt.block_on(async {
        let mut conn = inbox.connection().await.unwrap();
        let tx = inbox.begin(&mut conn).await.unwrap();
        let pid: i32 = tx
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        apply_commit(&tx, &commit).await.unwrap();
        // Kill the connection from another session, between the writes and the commit.
        let admin = e.db.raw_client().await;
        let killed: bool = admin
            .query_one("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap()
            .get(0);
        assert!(killed);
        assert!(
            tx.commit().await.is_err(),
            "the commit cannot succeed on a dead backend"
        );
    });
    nothing_persisted(&e, &ex);
    // The store recovers: the next commit works and the counter is intact.
    inbox.commit(plan(&w, "orders", 1, 2)).unwrap();
    assert_eq!(ex.notices().read_from(0, 10).unwrap().len(), 2);
}

#[test]
fn concurrent_commits_get_gapless_notice_numbers_a_reader_never_skips() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    const THREADS: usize = 8;
    const PER: usize = 25;
    let done = AtomicBool::new(false);

    let mut seen: Vec<u64> = Vec::new();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let feed = ex.notices();
            let mut after = 0;
            let mut got = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let batch = feed.read_from(after, 50).unwrap();
                for n in &batch {
                    got.push(n.cursor);
                    after = n.cursor;
                }
                if batch.is_empty() && done.load(Ordering::SeqCst) {
                    break;
                }
                assert!(Instant::now() < deadline, "the reader timed out");
            }
            got
        });
        let writers: Vec<_> = (0..THREADS)
            .map(|t| {
                let inbox = ex.inbox();
                let w = &w;
                scope.spawn(move || {
                    for n in 0..PER {
                        let name = format!("c{t}-{n}");
                        inbox.commit(plan(w, &name, 1, 1)).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        done.store(true, Ordering::SeqCst);
        seen = reader.join().unwrap();
    });
    let total = (THREADS * PER) as u64;
    // The reader, running while 200 transactions committed, saw every cursor exactly once and in
    // order: 1..=200, no gap, no repeat.
    assert_eq!(seen, (1..=total).collect::<Vec<_>>());
    assert_eq!(e.count(2, "spot_link_notice") as u64, total);
}

#[test]
fn many_conversations_keep_their_order_under_load() {
    // By default 200 conversations of 10 messages; with SPOT_LINK_PG_FULL=1 the full 1,000
    // conversations of 100 (100,000 messages, about twelve minutes on a laptop).
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    let full = std::env::var("SPOT_LINK_PG_FULL").as_deref() == Ok("1");
    let (conversations, messages): (u64, u64) = if full { (1_000, 100) } else { (200, 10) };
    const ROUND: u64 = 10;
    const THREADS: u64 = 8;

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let inbox = ex.inbox();
            let w = &w;
            scope.spawn(move || {
                // Each thread owns a slice of the conversations and interleaves them round by
                // round, so the notice feed mixes conversations while each stays in order.
                let mine: Vec<u64> = (0..conversations).filter(|c| c % THREADS == t).collect();
                for round in 0..messages / ROUND {
                    for c in &mine {
                        let name = format!("c{c:04}");
                        inbox
                            .commit(plan(w, &name, round * ROUND + 1, ROUND))
                            .unwrap();
                    }
                }
            });
        }
    });

    let total = conversations * messages;
    assert_eq!(e.count(2, "spot_link_inbox") as u64, total);
    assert_eq!(e.count(2, "spot_link_notice") as u64, total);
    rt.block_on(async {
        let schema = tenant_id(2).schema_name();
        let client = e.db.raw_client().await;
        client
            .batch_execute(&format!("SET search_path TO \"{schema}\""))
            .await
            .unwrap();
        // Every conversation holds exactly 1..=100.
        let broken: i64 = client
            .query_one(
                "SELECT count(*) FROM (SELECT conversation FROM spot_link_inbox GROUP BY conversation \
                 HAVING count(*) <> $1 OR min(seq) <> 1 OR max(seq) <> $1) t",
                &[&(messages as i64)],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(broken, 0);
        // Notice cursors are exactly 1..=total.
        let span: (i64, i64) = {
            let row = client
                .query_one("SELECT min(cursor), max(cursor) FROM spot_link_notice", &[])
                .await
                .unwrap();
            (row.get(0), row.get(1))
        };
        assert_eq!(span, (1, total as i64));
        // Within a conversation, notices follow the sequence numbers.
        let misordered: i64 = client
            .query_one(
                "SELECT count(*) FROM (SELECT i.seq, lag(i.seq) OVER \
                    (PARTITION BY i.conversation ORDER BY n.cursor) AS before \
                 FROM spot_link_notice n JOIN spot_link_inbox i ON i.id = n.message_id) t \
                 WHERE before IS NOT NULL AND seq <> before + 1",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(misordered, 0);
    });
    // And the consumer path reads them all, in pages, exactly once.
    let feed = ex.notices();
    let mut after = 0;
    let mut count = 0u64;
    let mut cursors = BTreeSet::new();
    loop {
        let page = feed.read_from(after, 5_000).unwrap();
        if page.is_empty() {
            break;
        }
        for n in &page {
            assert!(cursors.insert(n.cursor));
            after = n.cursor;
            count += 1;
        }
    }
    assert_eq!(count, total);
    let _ = ex.inbox().seen(&w.link_a.id, &w.message("c0000", 1, T0).id);
}
