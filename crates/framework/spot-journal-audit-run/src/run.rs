//! The audit run itself (ADR-061 s8).

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use spot_image_upcast::Registry;
use spot_journal_audit::{
    AuditResult, DerivedEntities, FingerprintReport, Position, RtoVerdict, VerifiedPosition,
    compare, compare_fingerprints, rto_check,
};
use spot_journal_rebuild::{RebuildKeys, RebuildOptions, RebuildOutcome, execute};
use spot_journal_rebuild_plan::{Base, ForkInfo, JournalCatalog, SegmentRange, Target, plan};
use spot_journal_snapshot::{SnapshotSigner, list_snapshots, mark_verified};
use spot_journal_store::{JournalStore, TenantId};

use crate::error::AuditRunError;
use crate::sides::{Clock, LiveSide, ScratchSide};

/// What one audit run needs besides the sides. One tenant per run, fixed here.
pub struct AuditOptions<'a> {
    /// The tenant audited; both sides must belong to it.
    pub tenant: TenantId,
    /// The journal source id the rebuild accepts (entries of another source are refused).
    pub source: String,
    /// The entity schema every image is upcast to.
    pub target_schema: u64,
    /// Per-entity target schemas that differ from `target_schema`.
    pub entity_targets: BTreeMap<String, u64>,
    /// The sealed segment ranges of the journal, from the verified manifests.
    pub segments: Vec<SegmentRange>,
    /// The recorded forks of the journal.
    pub forks: Vec<ForkInfo>,
    /// Entities derived from others (balances, indexes), left out of the record compare.
    pub derived: DerivedEntities,
    /// The tenant tier's recovery time objective, in milliseconds.
    pub rto_target_millis: u64,
    /// Use the latest valid snapshot even when no rebuild test verified it; a clean run then
    /// marks it verified, which needs [`Self::signer`].
    pub allow_untrusted_snapshot: bool,
    /// Signs the verified marker; only used on the untrusted-snapshot path.
    pub signer: Option<&'a dyn SnapshotSigner>,
    /// Cooperative cancellation of the rebuild.
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> AuditOptions<'a> {
    /// Options with no forks, no derived entities, trusted snapshots only and no signer.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        source: impl Into<String>,
        target_schema: u64,
        segments: Vec<SegmentRange>,
        rto_target_millis: u64,
    ) -> Self {
        Self {
            tenant,
            source: source.into(),
            target_schema,
            entity_targets: BTreeMap::new(),
            segments,
            forks: Vec::new(),
            derived: DerivedEntities::none(),
            rto_target_millis,
            allow_untrusted_snapshot: false,
            signer: None,
            cancel: None,
        }
    }
}

/// Where in the run a rebuild failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureStage {
    /// Listing the snapshots.
    Snapshots,
    /// Planning the rebuild (no chain, a gap, no base).
    Plan,
    /// Running the rebuild (verification, a missing upcaster, a segment problem, the sink).
    Rebuild,
}

/// A rebuild that did not complete. It names the failing crate's stable code and nothing else:
/// no record content, no object bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunFailure {
    /// Where it failed.
    pub stage: FailureStage,
    /// The stable code of the underlying error (`journal_rebuild.position_gap` and the like).
    pub code: &'static str,
    /// The position the run was pinned to.
    pub as_of: Position,
}

/// What happened to the snapshot the run started from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotMarking {
    /// The base was already trusted, or the run was not clean, or there was no snapshot base.
    NotNeeded,
    /// A clean run on an untrusted snapshot marked it verified.
    Marked,
    /// The run was clean but the marker could not be written; carries the snapshot error code.
    Failed(&'static str),
}

/// The result of a rebuild that completed and was compared.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditReport {
    /// The one position both sides were compared at.
    pub as_of: Position,
    /// The base the rebuild started from.
    pub base: Base,
    /// Whether the base snapshot had not been verified before this run.
    pub snapshot_was_untrusted: bool,
    /// What the rebuild did.
    pub rebuild: RebuildOutcome,
    /// The record compare: counts per entity and the first difference. Digests only.
    pub result: AuditResult,
    /// The fingerprint compare.
    pub fingerprints: FingerprintReport,
    /// How long plan and rebuild took, in milliseconds.
    pub rebuild_millis: u64,
    /// The rebuild time against the tier target.
    pub rto: RtoVerdict,
    /// Set exactly when the records and the fingerprints match.
    pub verified: Option<VerifiedPosition>,
    /// What happened to the base snapshot's verified marker.
    pub marking: SnapshotMarking,
}

impl AuditReport {
    /// Whether the records and the fingerprints both match. A breached RTO does not make a run
    /// unclean: the data is recoverable, only too slowly.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.result.is_match() && self.fingerprints.is_match()
    }
}

/// The outcome of one audit run.
#[derive(Clone, Debug, PartialEq)]
pub enum AuditRun {
    /// The rebuild completed and was compared with the live side.
    Completed(Box<AuditReport>),
    /// The rebuild itself failed; the schedule records this as its result row.
    Failed(RunFailure),
}

impl AuditRun {
    /// The report of a completed run.
    #[must_use]
    pub fn report(&self) -> Option<&AuditReport> {
        match self {
            Self::Completed(report) => Some(report),
            Self::Failed(_) => None,
        }
    }

    /// Whether the run completed and everything matched.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.report().is_some_and(AuditReport::is_clean)
    }

    /// The first differing record of a completed run that found one.
    #[must_use]
    pub fn first_difference(&self) -> Option<&spot_journal_audit::FirstDifference> {
        self.report()
            .and_then(|report| report.result.outcome.first_difference())
    }

    /// The verified position of a clean run, for the recovery report. Recording it is the
    /// caller's job: the run writes nothing but the snapshot marker.
    #[must_use]
    pub fn verified_position(&self) -> Option<VerifiedPosition> {
        self.report().and_then(|report| report.verified)
    }
}

fn check(
    options: &AuditOptions<'_>,
    live: &dyn LiveSide,
    scratch: &dyn ScratchSide,
) -> Result<(), AuditRunError> {
    if options.source.trim().is_empty() {
        return Err(AuditRunError::OptionsInvalid("the source is empty"));
    }
    if options.allow_untrusted_snapshot && options.signer.is_none() {
        return Err(AuditRunError::SignerMissing);
    }
    if live.tenant() != options.tenant.as_str() {
        return Err(AuditRunError::TenantMismatch { side: "live" });
    }
    if scratch.tenant() != options.tenant.as_str() {
        return Err(AuditRunError::TenantMismatch { side: "scratch" });
    }
    Ok(())
}

/// Runs the scheduled audit once.
///
/// The live head is read once and every later read of the live side is as of that position, so
/// a live database that moves during the run does not count. The latest trusted snapshot (or,
/// with [`AuditOptions::allow_untrusted_snapshot`], the latest valid one) plus the journal is
/// rebuilt into `scratch`; its summaries are compared with the live side's, derived entities
/// excluded, and the fingerprints are compared exactly. The rebuild time is measured with
/// `clock` against the tier target.
///
/// The scratch side is `discard`ed on every path once the sides have been accepted, including a
/// failed rebuild. The live side is only read. A rebuild that fails is an
/// [`AuditRun::Failed`], not an error.
///
/// # Errors
///
/// [`AuditRunError`] when the run is mis-wired (an empty source, a missing signer, a side of
/// another tenant) or a side's streams or fingerprints are unusable.
pub async fn run_audit(
    store: &dyn JournalStore,
    keys: &RebuildKeys<'_>,
    registry: &Registry,
    live: &dyn LiveSide,
    scratch: &mut dyn ScratchSide,
    options: &AuditOptions<'_>,
    clock: &dyn Clock,
) -> Result<AuditRun, AuditRunError> {
    check(options, live, scratch)?;
    let outcome = run_pinned(store, keys, registry, live, scratch, options, clock).await;
    scratch.discard();
    outcome
}

async fn run_pinned(
    store: &dyn JournalStore,
    keys: &RebuildKeys<'_>,
    registry: &Registry,
    live: &dyn LiveSide,
    scratch: &mut dyn ScratchSide,
    options: &AuditOptions<'_>,
    clock: &dyn Clock,
) -> Result<AuditRun, AuditRunError> {
    let as_of = live.head();
    let started = clock.now_millis();
    let failed = |stage, code| Ok(AuditRun::Failed(RunFailure { stage, code, as_of }));

    let listed = match list_snapshots(store, options.tenant.as_str(), keys.snapshot).await {
        Ok(listed) => listed,
        Err(error) => return failed(FailureStage::Snapshots, error.code()),
    };
    // A snapshot after the pinned position cannot be a base for it.
    let mut usable = listed.into_iter().filter(|s| s.position <= as_of.seq);
    let (snapshots, chosen_untrusted) = if options.allow_untrusted_snapshot {
        match usable.next_back() {
            Some(mut latest) => {
                let untrusted = !latest.verified;
                latest.verified = true;
                (vec![latest], untrusted.then_some(latest))
            }
            None => (Vec::new(), None),
        }
    } else {
        (usable.filter(|s| s.verified).collect(), None)
    };

    let catalog = JournalCatalog {
        snapshots,
        segments: options.segments.clone(),
        forks: options.forks.clone(),
    };
    let rebuild_plan = match plan(&catalog, None, Target::At(as_of.seq)) {
        Ok(rebuild_plan) => rebuild_plan,
        Err(error) => return failed(FailureStage::Plan, error.code()),
    };

    let mut rebuild_options = RebuildOptions::new(
        options.tenant.clone(),
        options.source.clone(),
        options.target_schema,
    );
    rebuild_options
        .entity_targets
        .clone_from(&options.entity_targets);
    rebuild_options.cancel = options.cancel;
    let rebuilt = match execute(
        store,
        keys,
        registry,
        &rebuild_plan,
        scratch,
        &rebuild_options,
    )
    .await
    {
        Ok(rebuilt) => rebuilt,
        Err(error) => return failed(FailureStage::Rebuild, error.code()),
    };
    let finished = clock.now_millis();
    let rebuild_millis = u64::try_from(finished.saturating_sub(started)).unwrap_or(0);

    let result = compare(live.summaries(as_of), scratch.summaries(), &options.derived)
        .map_err(AuditRunError::CompareFailed)?;
    let fingerprints = compare_fingerprints(&live.fingerprints(as_of), &scratch.fingerprints())
        .map_err(AuditRunError::CompareFailed)?;
    let clean = result.is_match() && fingerprints.is_match();

    let mut marking = SnapshotMarking::NotNeeded;
    if clean
        && let (
            Some(snapshot),
            Base::Snapshot {
                position,
                generation,
            },
        ) = (chosen_untrusted, rebuild_plan.base)
        && snapshot.position == position
        && snapshot.generation == generation
        && let Some(signer) = options.signer
    {
        marking = match mark_verified(
            store,
            options.tenant.as_str(),
            generation,
            position,
            keys.snapshot,
            signer,
        )
        .await
        {
            Ok(()) => SnapshotMarking::Marked,
            Err(error) => SnapshotMarking::Failed(error.code()),
        };
    }

    Ok(AuditRun::Completed(Box::new(AuditReport {
        as_of,
        base: rebuild_plan.base,
        snapshot_was_untrusted: chosen_untrusted.is_some(),
        rebuild: rebuilt,
        result,
        fingerprints,
        rebuild_millis,
        rto: rto_check(rebuild_millis, options.rto_target_millis),
        verified: clean.then_some(VerifiedPosition {
            position: as_of,
            at_millis: finished,
        }),
        marking,
    })))
}
