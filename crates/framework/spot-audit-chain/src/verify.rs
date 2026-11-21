//! The [`Verifier`]: recomputes hashes and links and checks checkpoints.
//!
//! A report never contains a payload: only the `seq`, the record kind and the failure.

use crate::chain::segment_bounds;
use crate::checkpoint::{Checkpoint, KeyRing};
use crate::entry::{ChainEntry, genesis_hash};
use crate::error::AuditError;
use crate::store::AuditStore;

/// What is wrong with an entry or checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// A field of the entry changed: its stored hash is not the hash of its contents.
    HashMismatch,
    /// The entry does not link to the one before it: one was removed, inserted or reordered.
    LinkBroken,
    /// The sequence numbers skip a value.
    SeqGap,
    /// The same sequence number appears twice.
    Duplicate,
    /// A checkpoint's signature does not verify (or it names another site).
    BadCheckpointSignature,
    /// A signed checkpoint names a `seq` or head hash the store no longer has (truncation).
    CheckpointAheadOfChain,
    /// A checkpoint is signed by a key the key ring does not know.
    UnknownKey,
}

impl FailureKind {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::HashMismatch => "audit.hash_mismatch",
            Self::LinkBroken => "audit.link_broken",
            Self::SeqGap => "audit.seq_gap",
            Self::Duplicate => "audit.duplicate",
            Self::BadCheckpointSignature => "audit.bad_checkpoint_signature",
            Self::CheckpointAheadOfChain => "audit.checkpoint_ahead_of_chain",
            Self::UnknownKey => "audit.unknown_key",
        }
    }
}

/// One finding, at an exact `seq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The sequence number of the entry (or of the checkpoint's head).
    pub seq: u64,
    /// What is wrong.
    pub kind: FailureKind,
    /// The kind of the audited record (never its payload); `None` for checkpoint findings.
    pub record_kind: Option<String>,
}

/// The outcome of a verification run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// The site.
    pub site: String,
    /// First sequence number asked for.
    pub from: u64,
    /// Last sequence number asked for.
    pub to: u64,
    /// Entries whose hash and link were checked.
    pub checked: u64,
    /// Every finding, in the order found.
    pub failures: Vec<Failure>,
}

impl VerifyReport {
    /// Whether nothing is wrong.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// Whether some finding has `kind`.
    #[must_use]
    pub fn has(&self, kind: FailureKind) -> bool {
        self.failures.iter().any(|f| f.kind == kind)
    }

    fn push(&mut self, seq: u64, kind: FailureKind, record_kind: Option<&str>) {
        let record_kind = record_kind.map(str::to_owned);
        let failure = Failure {
            seq,
            kind,
            record_kind,
        };
        if !self.failures.contains(&failure) {
            self.failures.push(failure);
        }
    }
}

/// Verifies chains against a [`KeyRing`] of checkpoint keys.
pub struct Verifier<'k> {
    keys: &'k KeyRing,
}

impl<'k> Verifier<'k> {
    /// A verifier that trusts the keys in `keys`.
    #[must_use]
    pub fn new(keys: &'k KeyRing) -> Self {
        Self { keys }
    }

    /// Checks one checkpoint's key and signature for `site`; `None` when it holds.
    #[must_use]
    pub fn check_checkpoint(&self, site: &str, cp: &Checkpoint) -> Option<FailureKind> {
        match self
            .keys
            .check(&cp.key_id, &cp.canonical_bytes(), &cp.signature)
        {
            None => Some(FailureKind::UnknownKey),
            Some(false) => Some(FailureKind::BadCheckpointSignature),
            Some(true) if cp.site != site => Some(FailureKind::BadCheckpointSignature),
            Some(true) => None,
        }
    }

    /// Recomputes every hash and link of `from..=to` (`from` is at least 1), then checks the
    /// store's latest checkpoint against the chain. The store only returns entries whose stored
    /// `seq` is in the range, so to verify a whole chain pass `u64::MAX` as `to`: an entry whose
    /// `seq` was altered to lie beyond a smaller `to` is then reported as a gap instead of being
    /// skipped.
    ///
    /// # Errors
    /// A store error.
    pub async fn verify_range(
        &self,
        store: &dyn AuditStore,
        site: &str,
        from: u64,
        to: u64,
    ) -> Result<VerifyReport, AuditError> {
        let mut report = self.verify_entries(store, site, from, to).await?;
        if let Some(cp) = store.latest_checkpoint(site).await? {
            self.check_against_chain(store, site, &cp, &mut report)
                .await?;
        }
        Ok(report)
    }

    /// Verifies one segment (see [`segment_bounds`]) on its own.
    ///
    /// # Errors
    /// A store error. A zero `len` verifies nothing.
    pub async fn verify_segment(
        &self,
        store: &dyn AuditStore,
        site: &str,
        index: u64,
        len: u64,
    ) -> Result<VerifyReport, AuditError> {
        match segment_bounds(index, len) {
            Some((first, last)) => self.verify_range(store, site, first, last).await,
            None => Ok(VerifyReport {
                site: site.to_owned(),
                from: 0,
                to: 0,
                checked: 0,
                failures: Vec::new(),
            }),
        }
    }

    /// Verifies only what is new since `trusted`, a checkpoint the caller already trusts (for
    /// example the last one a nightly run accepted, or one held by a third party). The trusted
    /// checkpoint's signature is checked, the store must still hold its head, and every entry
    /// after it is verified.
    ///
    /// # Errors
    /// A store error.
    pub async fn verify_incremental(
        &self,
        store: &dyn AuditStore,
        site: &str,
        trusted: &Checkpoint,
    ) -> Result<VerifyReport, AuditError> {
        let from = trusted.seq.saturating_add(1);
        let mut report = VerifyReport {
            site: site.to_owned(),
            from,
            to: u64::MAX,
            checked: 0,
            failures: Vec::new(),
        };
        if let Some(kind) = self.check_checkpoint(site, trusted) {
            report.push(trusted.seq, kind, None);
            return Ok(report);
        }
        self.check_against_chain(store, site, trusted, &mut report)
            .await?;
        let entries = self.verify_entries(store, site, from, u64::MAX).await?;
        report.checked = entries.checked;
        for f in entries.failures {
            report.push(f.seq, f.kind, f.record_kind.as_deref());
        }
        if let Some(latest) = store.latest_checkpoint(site).await?
            && latest != *trusted
        {
            self.check_against_chain(store, site, &latest, &mut report)
                .await?;
        }
        Ok(report)
    }

    async fn check_against_chain(
        &self,
        store: &dyn AuditStore,
        site: &str,
        cp: &Checkpoint,
        report: &mut VerifyReport,
    ) -> Result<(), AuditError> {
        if let Some(kind) = self.check_checkpoint(site, cp) {
            report.push(cp.seq, kind, None);
            return Ok(());
        }
        let at_seq = store.range(site, cp.seq, cp.seq).await?;
        if !at_seq
            .iter()
            .any(|e| e.seq == cp.seq && e.hash == cp.head_hash)
        {
            report.push(cp.seq, FailureKind::CheckpointAheadOfChain, None);
        }
        Ok(())
    }

    async fn verify_entries(
        &self,
        store: &dyn AuditStore,
        site: &str,
        from: u64,
        to: u64,
    ) -> Result<VerifyReport, AuditError> {
        let from = from.max(1);
        let mut report = VerifyReport {
            site: site.to_owned(),
            from,
            to,
            checked: 0,
            failures: Vec::new(),
        };
        let anchored = from > 1;
        let fetch_from = if anchored { from - 1 } else { from };
        let mut entries = store.range(site, fetch_from, to).await?;

        let mut last_seq: Option<u64> = None;
        let mut expected_prev = if anchored {
            None
        } else {
            Some(genesis_hash(site))
        };
        if anchored && entries.first().is_some_and(|e| e.seq == fetch_from) {
            let anchor: ChainEntry = entries.remove(0);
            if anchor.recompute_hash() != anchor.hash {
                report.push(
                    anchor.seq,
                    FailureKind::HashMismatch,
                    Some(&anchor.record.kind),
                );
            }
            last_seq = Some(anchor.seq);
            expected_prev = Some(anchor.hash);
        }
        let mut expected_seq = from;
        for e in &entries {
            let kind = Some(e.record.kind.as_str());
            report.checked = report.checked.saturating_add(1);
            if last_seq == Some(e.seq) {
                report.push(e.seq, FailureKind::Duplicate, kind);
                continue;
            }
            if last_seq.is_some_and(|l| e.seq < l) {
                report.push(e.seq, FailureKind::LinkBroken, kind);
            } else if e.seq > expected_seq {
                report.push(e.seq, FailureKind::SeqGap, kind);
            }
            if expected_prev.is_some_and(|p| e.prev_hash != p) {
                report.push(e.seq, FailureKind::LinkBroken, kind);
            }
            if e.recompute_hash() != e.hash {
                report.push(e.seq, FailureKind::HashMismatch, kind);
            }
            last_seq = Some(e.seq);
            expected_prev = Some(e.hash);
            expected_seq = e.seq.saturating_add(1);
        }
        Ok(report)
    }
}
