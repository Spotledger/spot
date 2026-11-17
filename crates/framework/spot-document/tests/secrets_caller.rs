//! The `operator` secret caller (WP-SEC-03.T12): the key rotation sweep is journalled as the
//! operator tool, not as the bootstrap's `seed`. The caller is an audit label, never a
//! permission; rows written by older builds as `seed` stay readable; a caller name this build
//! does not know is refused when the journal is read back (the behaviour of the engine today).
#![allow(
    missing_docs,
    dead_code,
    unused_imports,
    unused_macros,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod common;

#[path = "secrets/support.rs"]
mod support;

use spot_storage_contract::Outbox;
use support::*;

const TOPIC: &str = "sys_secret_audit";

#[test]
fn every_caller_round_trips_through_its_name() {
    let all = [
        SecretCaller::LoginService,
        SecretCaller::Seed,
        SecretCaller::Operator,
        SecretCaller::Test,
    ];
    for caller in all {
        assert_eq!(SecretCaller::from_name(caller.as_str()), Some(caller));
    }
    assert_eq!(SecretCaller::Operator.as_str(), "operator");
    let mut names: Vec<_> = all.iter().map(|c| c.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all.len(), "two callers share a name");
    // Names are exact: nothing is matched loosely.
    assert_eq!(SecretCaller::from_name("Operator"), None);
    assert_eq!(SecretCaller::from_name(""), None);
    assert_eq!(SecretCaller::from_name("operator "), None);
}

/// A copy of the first journal event of `op` (with a fresh id), whose `caller` is `caller`:
/// what an older build, or a build that knows other callers, would have written.
async fn inject_with_caller<B: StorageBackend + ChangeFeed>(r: &Rig<B>, op: &str, caller: &str) {
    let records = r
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 100_000)
        .await
        .unwrap();
    let source = records
        .iter()
        .map(|record| &record.event)
        .find(|event| {
            event.topic.as_str() == TOPIC
                && serde_json::from_str::<serde_json::Value>(&event.payload)
                    .unwrap()
                    .pointer("/data/op")
                    .and_then(serde_json::Value::as_str)
                    == Some(op)
        })
        .expect("a journal event of the operation");
    let mut event = source.clone();
    event.id = RecordId::new_v7();
    let mut payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    payload["data"]["caller"] = serde_json::Value::String(caller.to_owned());
    event.payload = serde_json::to_string(&payload).unwrap();
    let mut tx = r
        .engine
        .backend()
        .begin(
            catalog(),
            TxOptions::new(n("raw_enqueue"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    tx.enqueue(&event).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn an_operator_rewrap_is_journalled_as_operator_and_never_as_seed() {
    let r = rig_fake().await.unwrap();
    let seed = r.engine.secret_access(SecretCaller::Seed, &inv());
    let operator = r.engine.secret_access(SecretCaller::Operator, &inv());
    assert_eq!(operator.caller(), SecretCaller::Operator);

    let (id, v1) = r.account("rot").await;
    r.engine
        .set_secret(&seed, "account", id, "token", s(TOKEN), v1)
        .await
        .unwrap();
    r.engine
        .rewrap_secrets(&operator, "account", RewrapBatch::first(10))
        .await
        .unwrap();

    let journal = r.engine.secret_audit_journal(&operator).await.unwrap();
    let rewraps: Vec<_> = journal
        .iter()
        .filter(|e| e.op == SecretOp::Rewrap)
        .collect();
    assert!(!rewraps.is_empty(), "the rewrap was not journalled");
    for entry in &rewraps {
        assert_eq!(entry.caller, SecretCaller::Operator);
    }
    assert!(
        !rewraps.iter().any(|e| e.caller == SecretCaller::Seed),
        "a rewrap was journalled as seed"
    );
    // The seeding write stays `seed`: the two are told apart.
    assert!(
        journal
            .iter()
            .any(|e| e.op == SecretOp::Set && e.caller == SecretCaller::Seed)
    );
    // The in-process view agrees.
    assert!(
        r.engine
            .secret_audit_log()
            .iter()
            .filter(|e| e.op == SecretOp::Rewrap)
            .all(|e| e.caller == SecretCaller::Operator)
    );
    // The label is not a permission and is not in the wire text of a value.
    let text = format!("{journal:?}");
    assert!(!text.contains(TOKEN));
}

#[tokio::test]
async fn a_row_journalled_as_seed_by_older_code_still_reads_back() {
    let r = rig_fake().await.unwrap();
    let operator = r.engine.secret_access(SecretCaller::Operator, &inv());
    let (id, v1) = r.account("old").await;
    let seed = r.engine.secret_access(SecretCaller::Seed, &inv());
    r.engine
        .set_secret(&seed, "account", id, "token", s(TOKEN), v1)
        .await
        .unwrap();
    r.engine
        .rewrap_secrets(&operator, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    inject_with_caller(&r, "rewrap", "seed").await;

    let journal = r.engine.secret_audit_journal(&operator).await.unwrap();
    let rewraps: Vec<_> = journal
        .iter()
        .filter(|e| e.op == SecretOp::Rewrap)
        .collect();
    assert_eq!(rewraps.len(), 2);
    assert_eq!(rewraps[0].caller, SecretCaller::Operator);
    assert_eq!(rewraps[1].caller, SecretCaller::Seed);
}

#[tokio::test]
async fn a_row_with_an_unknown_caller_fails_the_journal_read_as_it_does_today() {
    let r = rig_fake().await.unwrap();
    let operator = r.engine.secret_access(SecretCaller::Operator, &inv());
    let (id, v1) = r.account("future").await;
    r.engine
        .set_secret(&operator, "account", id, "token", s(TOKEN), v1)
        .await
        .unwrap();
    r.engine
        .rewrap_secrets(&operator, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert!(r.engine.secret_audit_journal(&operator).await.is_ok());
    inject_with_caller(&r, "rewrap", "some_future_caller").await;

    let err = r
        .engine
        .secret_audit_journal(&operator)
        .await
        .expect_err("a caller this build does not know is refused, not guessed");
    assert_eq!(err.code, "document.internal_error");
}
