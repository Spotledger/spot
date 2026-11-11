//! Unit tests. They reach the store directly to tamper with an entry, which no public API can.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod lifecycle;
mod performance;
mod permission;
mod tamper;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spot_audit_chain::{
    AppendOutcome, AuditChain, AuditError, AuditStore, ChainEntry, Checkpoint, CheckpointPolicy,
    KeyRing, LocalSigner, StoreFuture,
};

use crate::recorder::ChainRunRecorder;

/// The site every test uses.
pub(crate) const SITE: &str = "site-1";

/// An [`AuditStore`] the tests can tamper with, since no public API can.
#[derive(Default)]
pub(crate) struct TestStore {
    entries: Mutex<HashMap<String, Vec<ChainEntry>>>,
    checkpoint: Mutex<Option<Checkpoint>>,
}

impl TestStore {
    fn lock_entries(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Vec<ChainEntry>>>, AuditError> {
        self.entries
            .lock()
            .map_err(|_| AuditError::Store("the test store lock is poisoned".to_owned()))
    }

    /// Alters the entry at `seq` (the whole chain is one site in these tests).
    pub(crate) fn tamper(&self, seq: u64, change: impl FnOnce(&mut ChainEntry)) {
        let mut guard = self.entries.lock().unwrap();
        for chain in guard.values_mut() {
            if let Some(entry) = chain.iter_mut().find(|e| e.seq == seq) {
                change(entry);
                return;
            }
        }
    }

    /// Removes the entry at `seq`.
    pub(crate) fn remove(&self, seq: u64) {
        let mut guard = self.entries.lock().unwrap();
        for chain in guard.values_mut() {
            chain.retain(|e| e.seq != seq);
        }
    }
}

impl AuditStore for TestStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            Ok(self
                .lock_entries()?
                .get(site)
                .and_then(|chain| chain.last())
                .cloned())
        })
    }

    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async move {
            let mut guard = self.lock_entries()?;
            let chain = guard.entry(site.to_owned()).or_default();
            let head_seq = chain.last().map_or(0, |e| e.seq);
            if head_seq != expected_seq {
                return Ok(AppendOutcome::HeadMoved);
            }
            let prev_ok = chain.last().is_none_or(|head| head.hash == entry.prev_hash);
            if entry.seq != expected_seq.saturating_add(1) || !prev_ok {
                return Err(AuditError::Store(
                    "the entry does not extend the head".to_owned(),
                ));
            }
            chain.push(entry);
            Ok(AppendOutcome::Appended)
        })
    }

    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            Ok(self
                .lock_entries()?
                .get(site)
                .map(|chain| {
                    chain
                        .iter()
                        .filter(|e| e.seq >= from && e.seq <= to)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            *self.checkpoint.lock().map_err(|_| {
                AuditError::Store("the test checkpoint lock is poisoned".to_owned())
            })? = Some(checkpoint);
            Ok(())
        })
    }

    fn latest_checkpoint<'a>(&'a self, _site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        Box::pin(async move {
            Ok(self
                .checkpoint
                .lock()
                .map_err(|_| AuditError::Store("the test checkpoint lock is poisoned".to_owned()))?
                .clone())
        })
    }
}

/// A recorder over a [`TestStore`], and the key ring that verifies it.
pub(crate) fn recorder(seed: u8) -> (ChainRunRecorder<TestStore>, KeyRing) {
    let signer = Arc::new(LocalSigner::from_seed([seed; 32]));
    let mut ring = KeyRing::new();
    ring.add(&signer.public_key()).unwrap();
    let chain = AuditChain::new(
        TestStore::default(),
        SITE,
        signer,
        CheckpointPolicy { every: 0 },
    );
    (ChainRunRecorder::new(chain), ring)
}

/// A plan summary with a few changes, for tests.
pub(crate) fn summary() -> crate::run::PlanSummary {
    crate::run::PlanSummary {
        risk: "needs_review".to_owned(),
        changes: 4,
        schema: 1,
        metadata: 2,
        patches: 0,
        behaviour: 1,
    }
}
