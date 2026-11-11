//! The backend-independent contract suite for [`AuditStore`].
//!
//! Every store (the in-memory one, the PostgreSQL one of WP-AI-11.T2) runs [`run`] against a
//! fresh or shared backend; it uses sites that start with the given prefix, so it can share a
//! database with other tests. A failure is a plain message naming the broken rule.

use crate::checkpoint::{Checkpoint, CheckpointSigner, LocalSigner};
use crate::entry::{ChainEntry, genesis_hash};
use crate::record::AuditRecord;
use crate::store::{AppendOutcome, AuditStore};

fn record(n: i64) -> AuditRecord {
    AuditRecord {
        kind: "contractprobe".to_owned(),
        actor: "system".to_owned(),
        on_behalf_of: None,
        subject: None,
        at: n,
        payload: Default::default(),
        masked: true,
    }
}

fn check(ok: bool, rule: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(rule.to_owned()) }
}

fn failed<E: std::fmt::Display>(e: E) -> String {
    format!("store error: {e}")
}

/// Runs the suite.
///
/// # Errors
/// The rule that does not hold.
pub async fn run(store: &dyn AuditStore, site_prefix: &str) -> Result<(), String> {
    let a = format!("{site_prefix}-a");
    let b = format!("{site_prefix}-b");

    check(
        store.head(&a).await.map_err(failed)?.is_none(),
        "an empty chain has no head",
    )?;
    check(
        store.latest_checkpoint(&a).await.map_err(failed)?.is_none(),
        "a site without checkpoints has no latest checkpoint",
    )?;

    // Append 1..=3, each extending the head.
    let mut prev = genesis_hash(&a);
    let mut written = Vec::new();
    for n in 1..=3_u64 {
        let entry = ChainEntry::new(n, prev, record(i64::try_from(n).unwrap_or(0)));
        prev = entry.hash;
        let outcome = store
            .append_if_head(&a, n - 1, entry.clone())
            .await
            .map_err(failed)?;
        check(
            outcome == AppendOutcome::Appended,
            "an entry extending the head is appended",
        )?;
        written.push(entry);
    }
    check(
        store.head(&a).await.map_err(failed)?.as_ref() == written.last(),
        "head is the newest appended entry",
    )?;

    // Compare-and-set: a writer that read an older head loses and writes nothing.
    let stale = ChainEntry::new(2, written[0].hash, record(99));
    check(
        store.append_if_head(&a, 1, stale).await.map_err(failed)? == AppendOutcome::HeadMoved,
        "an append against a stale head reports HeadMoved",
    )?;
    check(
        store.range(&a, 1, 100).await.map_err(failed)? == written,
        "a lost append writes nothing",
    )?;

    // A matching head with an entry that does not extend it is refused.
    let bad_seq = ChainEntry::new(9, written[2].hash, record(7));
    check(
        store.append_if_head(&a, 3, bad_seq).await.is_err(),
        "an entry with a wrong seq is refused",
    )?;
    let bad_link = ChainEntry::new(4, genesis_hash(&a), record(7));
    check(
        store.append_if_head(&a, 3, bad_link).await.is_err(),
        "an entry with a wrong prev_hash is refused",
    )?;

    // Ranges are inclusive, ordered and clamped.
    check(
        store.range(&a, 2, 3).await.map_err(failed)? == written[1..],
        "range is inclusive on both ends and ordered",
    )?;
    check(
        store.range(&a, 4, 10).await.map_err(failed)?.is_empty(),
        "range past the head is empty",
    )?;

    // Sites are independent.
    check(
        store.head(&b).await.map_err(failed)?.is_none(),
        "another site's chain is untouched",
    )?;
    let first_b = ChainEntry::new(1, genesis_hash(&b), record(1));
    check(
        store.append_if_head(&b, 0, first_b).await.map_err(failed)? == AppendOutcome::Appended,
        "each site has its own chain starting at 1",
    )?;

    // Checkpoints: the newest wins; an older one is ignored.
    let signer = LocalSigner::from_seed([7; 32]);
    let signer: &dyn CheckpointSigner = &signer;
    let cp3 = Checkpoint::sign(signer, &a, 3, written[2].hash, 30).map_err(failed)?;
    let cp2 = Checkpoint::sign(signer, &a, 2, written[1].hash, 20).map_err(failed)?;
    store.put_checkpoint(cp3.clone()).await.map_err(failed)?;
    store.put_checkpoint(cp2).await.map_err(failed)?;
    check(
        store.latest_checkpoint(&a).await.map_err(failed)?.as_ref() == Some(&cp3),
        "latest_checkpoint is the one with the highest seq",
    )?;
    check(
        store.latest_checkpoint(&b).await.map_err(failed)?.is_none(),
        "checkpoints are per site",
    )?;
    Ok(())
}
