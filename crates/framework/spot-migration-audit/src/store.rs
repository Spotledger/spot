//! Reading runs back: the [`RunStore`] query API and its chain-backed implementation.
//!
//! The store folds the run's lifecycle events out of the tamper-evident chain. It caches the
//! fold keyed on the chain head's sequence number, so a console that lists and re-lists does not
//! re-scan the chain until something new is appended. It never reads a clock.

use std::collections::BTreeMap;
use std::sync::Mutex;

use futures::future::BoxFuture;
use spot_audit_chain::{AuditStore, ChainEntry, Verifier, VerifyReport};

use crate::error::MigrationAuditError;
use crate::events::{Event, decode, fold};
use crate::run::{MigrationRun, RunEvidence, RunFilter, RunId};

/// Reading runs and their evidence.
pub trait RunStore: Send + Sync {
    /// One run and its chain entries, or `None` when the id is unknown.
    ///
    /// # Errors
    /// [`MigrationAuditError::Chain`] when the store fails, or
    /// [`MigrationAuditError::RecordInvalid`] when a stored run event is malformed.
    fn get<'a>(
        &'a self,
        id: &'a RunId,
    ) -> BoxFuture<'a, Result<Option<RunEvidence>, MigrationAuditError>>;

    /// Every run passing `filter`, in chain (start) order.
    ///
    /// # Errors
    /// As [`RunStore::get`].
    fn list<'a>(
        &'a self,
        filter: RunFilter,
    ) -> BoxFuture<'a, Result<Vec<MigrationRun>, MigrationAuditError>>;
}

/// A run and its entries, folded from the chain.
struct Index {
    head_seq: u64,
    runs: Vec<MigrationRun>,
    evidence: BTreeMap<RunId, Vec<ChainEntry>>,
}

/// The folded view of one site's chain.
pub struct ChainRunStore<'a, S: AuditStore> {
    store: &'a S,
    site: String,
    cache: Mutex<Option<Index>>,
}

impl<'a, S: AuditStore> ChainRunStore<'a, S> {
    /// A store reading `site`'s runs from `store`.
    #[must_use]
    pub fn new(store: &'a S, site: impl Into<String>) -> Self {
        Self {
            store,
            site: site.into(),
            cache: Mutex::new(None),
        }
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Index>>, MigrationAuditError> {
        self.cache.lock().map_err(|_| MigrationAuditError::Chain {
            message: "the run-store cache lock is poisoned".to_owned(),
        })
    }

    fn cache_is_current(&self, head_seq: u64) -> Result<bool, MigrationAuditError> {
        Ok(self
            .lock()?
            .as_ref()
            .is_some_and(|index| index.head_seq == head_seq))
    }

    /// Rebuilds the fold when the chain head has moved (or nothing is cached yet).
    async fn ensure(&self) -> Result<(), MigrationAuditError> {
        let head = self.store.head(&self.site).await?;
        let head_seq = head.as_ref().map_or(0, |entry| entry.seq);
        if self.cache_is_current(head_seq)? {
            return Ok(());
        }
        let entries = self.store.range(&self.site, 1, u64::MAX).await?;
        let index = build_index(entries, head_seq)?;
        *self.lock()? = Some(index);
        Ok(())
    }

    /// Verifies the whole chain this store reads.
    ///
    /// # Errors
    /// [`MigrationAuditError::Chain`] when the store fails.
    pub async fn verify(
        &self,
        verifier: &Verifier<'_>,
    ) -> Result<VerifyReport, MigrationAuditError> {
        Ok(verifier
            .verify_range(self.store, &self.site, 1, u64::MAX)
            .await?)
    }

    /// The sequence number of the chain head (0 for an empty chain).
    ///
    /// # Errors
    /// [`MigrationAuditError::Chain`] when the store fails.
    pub async fn head_seq(&self) -> Result<u64, MigrationAuditError> {
        Ok(self
            .store
            .head(&self.site)
            .await?
            .as_ref()
            .map_or(0, |entry| entry.seq))
    }
}

impl<S: AuditStore> RunStore for ChainRunStore<'_, S> {
    fn get<'a>(
        &'a self,
        id: &'a RunId,
    ) -> BoxFuture<'a, Result<Option<RunEvidence>, MigrationAuditError>> {
        Box::pin(async move {
            self.ensure().await?;
            let found = {
                let guard = self.lock()?;
                guard.as_ref().and_then(|index| {
                    let entries = index.evidence.get(id)?;
                    let run = index.runs.iter().find(|run| &run.id == id)?;
                    Some(RunEvidence {
                        run: run.clone(),
                        entries: entries.clone(),
                    })
                })
            };
            Ok(found)
        })
    }

    fn list<'a>(
        &'a self,
        filter: RunFilter,
    ) -> BoxFuture<'a, Result<Vec<MigrationRun>, MigrationAuditError>> {
        Box::pin(async move {
            self.ensure().await?;
            let runs = {
                let guard = self.lock()?;
                match guard.as_ref() {
                    None => Vec::new(),
                    Some(index) => index
                        .runs
                        .iter()
                        .filter(|run| filter.matches(run))
                        .cloned()
                        .collect(),
                }
            };
            Ok(runs)
        })
    }
}

fn build_index(entries: Vec<ChainEntry>, head_seq: u64) -> Result<Index, MigrationAuditError> {
    let mut evidence: BTreeMap<RunId, Vec<ChainEntry>> = BTreeMap::new();
    let mut decoded: Vec<Event> = Vec::new();
    for entry in entries {
        if let Some(event) = decode(&entry.record)? {
            evidence
                .entry(event.run_id().clone())
                .or_default()
                .push(entry);
            decoded.push(event);
        }
    }
    let runs = fold(&decoded);
    Ok(Index {
        head_seq,
        runs,
        evidence,
    })
}
