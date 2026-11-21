//! `rebuild_site`: one offline rebuild, in the order ADR-061 s6 gives.
//!
//! 1. refuse a wrong tenant, a running site and a database left half-built by another plan;
//! 2. read the journal's catalogue, resolve the target, plan (the planner refuses a gap, a missing
//!    base, a target inside an abandoned generation);
//! 3. `--dry-run` stops here and reports the plan;
//! 4. check every derived entity has a projector, **before the first write**;
//! 5. run the executor into the checkpointing sink (resume from the checkpoint, state
//!    `Rebuilding`);
//! 6. adopt the journal's outbox position (or open the fork's new generation and record the fork
//!    in the manifest), **then** write the audit events, so the first new event is
//!    `(generation, seq + 1)`;
//! 7. re-run the projectors, run the invariant suite, mark the rebuild complete.
//!
//! A failure anywhere leaves the checkpoint and the `Rebuilding` state: the site refuses to serve
//! the database until the rebuild finishes or the target is explicitly discarded.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};
use spot_events_contract::envelope::Position;
use spot_journal_rebuild::{ApplySink, RebuildError, RebuildOutcome};
use spot_journal_rebuild_plan::{
    Backup, Base, ForkPoint, JournalCatalog, RebuildPlan, Target, explain, plan,
};

use crate::checkpoint::{Checkpoint, CheckpointStore, Fut, State};
use crate::error::HostError;
use crate::projector::Projectors;
use crate::request::{Mode, RebuildRequest, TargetSpec};
use crate::sink::{CheckpointingSink, SinkCounts};

/// A boxed future for the executor seam.
pub type RebuildFut<'a> =
    Pin<Box<dyn Future<Output = Result<RebuildOutcome, RebuildError>> + Send + 'a>>;

/// Whether a live node holds the site.
pub trait SiteGuard: Send + Sync {
    /// `Ok` when no live node holds the site's lease (the maintenance lock is then held for the
    /// run); [`HostError::SiteRunning`] otherwise.
    fn ensure_stopped<'a>(&'a self, site: &'a str) -> Fut<'a, ()>;
}

/// The journal as the host sees it.
pub trait JournalSide: Send + Sync {
    /// The tenant of the journal (checked against every manifest and snapshot index signature
    /// before the first write).
    fn tenant(&self) -> &str;
    /// What the journal holds. A manifest or snapshot index of another tenant is
    /// [`HostError::TenantMismatch`].
    fn catalog(&self) -> Fut<'_, JournalCatalog>;
    /// The last position recorded before `millis`, for `before <timestamp>`.
    fn resolve_before(&self, millis: i64) -> Fut<'_, Option<(u64, u64)>>;
    /// Records the fork in the manifest through the relay's `open_fork`.
    fn record_fork(&self, fork: ForkPoint, new_generation: u64) -> Fut<'_, ()>;
}

/// Runs the executor: verification, snapshot, segments, upcasters, into `sink`.
pub trait Rebuilder: Send + Sync {
    /// Executes `plan` into `sink`.
    fn execute<'a>(&'a self, plan: &'a RebuildPlan, sink: &'a mut dyn ApplySink) -> RebuildFut<'a>;
}

/// Moves the feed's head (the outbox the rebuilt database continues).
pub trait FeedAdmin: Send + Sync {
    /// Adopts `(generation, seq)` as the head (never backwards).
    fn adopt<'a>(&'a self, generation: u64, seq: u64) -> Fut<'a, ()>;
    /// Opens `generation` at seq 0 for a fork.
    fn open_generation<'a>(&'a self, generation: u64) -> Fut<'a, ()>;
}

/// Writes `sys_journal_admin`.
pub trait AdminAudit: Send + Sync {
    /// One admin event. `target` is a position text, `code` a catalogue code (not for `ok`).
    fn record<'a>(
        &'a self,
        action: &'static str,
        target: Option<String>,
        outcome: &'static str,
        code: Option<String>,
        dry_run: bool,
    ) -> Fut<'a, ()>;
}

/// One invariant's result: a name, whether it held and counts, never a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InvariantResult {
    /// The invariant (`document_counts`, `graph_edges`, `financial_fingerprint`).
    pub name: String,
    /// Whether it held.
    pub held: bool,
    /// How many items it checked.
    pub checked: u64,
    /// How many did not hold.
    pub violations: u64,
}

/// The invariant suite run after the projectors.
pub trait Invariants: Send + Sync {
    /// Runs the suite.
    fn run(&self) -> Fut<'_, Vec<InvariantResult>>;
}

/// Everything a rebuild needs. The host (or the CLI) builds it once.
pub struct Deps {
    /// Is a node running?
    pub site: Arc<dyn SiteGuard>,
    /// The journal.
    pub journal: Arc<dyn JournalSide>,
    /// The executor.
    pub rebuilder: Arc<dyn Rebuilder>,
    /// Builds the sink that writes the database (fresh per attempt).
    pub sink: Arc<dyn Fn() -> Box<dyn ApplySink> + Send + Sync>,
    /// The checkpoint store.
    pub checkpoints: Arc<dyn CheckpointStore>,
    /// The feed.
    pub feed: Arc<dyn FeedAdmin>,
    /// The projectors.
    pub projectors: Projectors,
    /// The derived entities owners flagged (each needs a projector).
    pub derived: Vec<String>,
    /// The invariant suite.
    pub invariants: Arc<dyn Invariants>,
    /// The audit.
    pub audit: Arc<dyn AdminAudit>,
}

/// What a rebuild reports: counts and positions, never content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RebuildReport {
    /// The mode that ran.
    pub mode: Mode,
    /// The plan, as the operator-readable lines of the planner.
    pub plan: Vec<String>,
    /// Whether this was a dry run (nothing was written).
    pub dry_run: bool,
    /// Images applied by this run.
    pub applied_images: u64,
    /// Deletes applied by this run.
    pub applied_deletes: u64,
    /// Items skipped because a saved checkpoint covered them.
    pub resumed_skipped: u64,
    /// Domain entries the executor read and did not apply (available, not applied: A.0.1).
    pub skipped_domain_events: u64,
    /// The last position applied, `generation:seq`.
    pub last_position: Option<String>,
    /// The generation the rebuilt stream continues in, when it forked.
    pub new_generation: Option<u64>,
    /// The projectors that ran.
    pub projectors: Vec<String>,
    /// The invariant results.
    pub invariants: Vec<InvariantResult>,
    /// Non-fatal notes (counts and codes).
    pub warnings: Vec<String>,
}

/// The stable hash of a plan.
#[must_use]
pub fn plan_hash(plan: &RebuildPlan) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{plan:?}").as_bytes());
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

fn mode_of(plan: &RebuildPlan) -> Mode {
    if plan.fork_point.is_some() {
        Mode::PointInTime
    } else if matches!(plan.base, Base::Backup { .. }) {
        Mode::RollForward
    } else {
        Mode::Full
    }
}

fn position_text(position: Position) -> String {
    format!("{}:{}", position.generation, position.seq)
}

/// Rebuilds `request.site`. See the module docs for the order.
///
/// # Errors
/// [`HostError`]; on a failure after the first write the checkpoint and the `Rebuilding` state
/// stay for a resume or an explicit discard.
pub async fn rebuild_site(
    deps: &Deps,
    request: &RebuildRequest,
) -> Result<RebuildReport, HostError> {
    if request.tenant != deps.journal.tenant() {
        return Err(HostError::TenantMismatch);
    }
    deps.site.ensure_stopped(&request.site).await?;
    let catalog = deps.journal.catalog().await?;
    let target = match request.target {
        TargetSpec::Latest => Target::End,
        TargetSpec::At { generation, seq } => Target::In {
            generation,
            position: seq,
        },
        TargetSpec::Before { millis } => {
            let (generation, seq) = deps
                .journal
                .resolve_before(millis)
                .await?
                .ok_or(HostError::TargetUnresolved)?;
            Target::In {
                generation,
                position: seq,
            }
        }
    };
    let backup = match request.backup_head {
        None => None,
        Some((generation, seq)) => {
            let listed = catalog
                .segments
                .iter()
                .any(|s| s.generation == generation && s.first_seq <= seq && seq <= s.last_seq)
                || catalog
                    .snapshots
                    .iter()
                    .any(|s| s.generation == generation && s.position == seq);
            if !listed {
                return Err(HostError::BaseNotInJournal);
            }
            Some(Backup::in_generation(seq, generation))
        }
    };
    let planned = plan(&catalog, backup.as_ref(), target)
        .map_err(|error| HostError::PlanRefused(error.code()))?;
    let hash = plan_hash(&planned);
    let lines: Vec<String> = explain(&planned)
        .iter()
        .map(|line| {
            let params: Vec<String> = line
                .params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!("{}({})", line.key, params.join(", "))
        })
        .collect();
    let mode = mode_of(&planned);
    if request.dry_run {
        deps.audit
            .record("rebuild_planned", None, "ok", None, true)
            .await?;
        return Ok(RebuildReport {
            mode,
            plan: lines,
            dry_run: true,
            applied_images: 0,
            applied_deletes: 0,
            resumed_skipped: 0,
            skipped_domain_events: 0,
            last_position: None,
            new_generation: planned.new_generation,
            projectors: Vec::new(),
            invariants: Vec::new(),
            warnings: vec!["dry run: the database was not touched".to_owned()],
        });
    }
    deps.projectors.check(&deps.derived)?;
    let resume = match deps.checkpoints.load(&request.site).await? {
        Some(saved) if saved.state == State::Complete => None,
        Some(saved) if saved.plan_hash != hash => return Err(HostError::CheckpointForOtherPlan),
        Some(saved) if saved.generation == 0 => None,
        Some(saved) => Some(Position {
            generation: saved.generation,
            seq: saved.seq,
        }),
        None => None,
    };
    deps.audit
        .record("rebuild_started", None, "ok", None, false)
        .await?;
    let mut sink = CheckpointingSink::new(
        (deps.sink)(),
        Arc::clone(&deps.checkpoints),
        &request.site,
        &hash,
        request.checkpoint_every,
        resume,
    );
    let outcome = match deps.rebuilder.execute(&planned, &mut sink).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let code = error.code().to_owned();
            deps.audit
                .record("rebuild_failed", None, "failed", Some(code.clone()), false)
                .await
                .ok();
            return Err(HostError::RebuildFailed(code));
        }
    };
    let counts: SinkCounts = sink.counts();
    // Adopt the journal's positions (or open the fork), then audit: the audit's own outbox
    // write must come after the head moved, so the first new event is (generation, seq + 1).
    let last = outcome.last_position;
    match (planned.new_generation, planned.fork_point) {
        (Some(new_generation), Some(fork)) => {
            deps.journal.record_fork(fork, new_generation).await?;
            deps.feed.open_generation(new_generation).await?;
        }
        _ => deps.feed.adopt(last.generation, last.seq).await?,
    }
    let mut ran = Vec::new();
    for projector in deps.projectors.all() {
        projector
            .rebuild()
            .await
            .map_err(|_| HostError::ProjectorFailed(projector.name().to_owned()))?;
        ran.push(projector.name().to_owned());
    }
    let invariants = deps.invariants.run().await?;
    if let Some(bad) = invariants.iter().find(|i| !i.held) {
        deps.audit
            .record(
                "rebuild_failed",
                Some(position_text(last)),
                "failed",
                Some("journal_rebuild_host.invariant_failed".to_owned()),
                false,
            )
            .await
            .ok();
        return Err(HostError::InvariantFailed(bad.name.clone()));
    }
    deps.checkpoints
        .save(
            &request.site,
            &Checkpoint {
                plan_hash: hash,
                generation: last.generation,
                seq: last.seq,
                state: State::Complete,
            },
        )
        .await?;
    if let Some(fork) = planned.fork_point {
        deps.audit
            .record(
                "fork_opened",
                Some(format!(
                    "{}:{}",
                    fork.abandoned_generation, fork.from_position
                )),
                "ok",
                None,
                false,
            )
            .await?;
    }
    deps.audit
        .record(
            "rebuild_completed",
            Some(position_text(last)),
            "ok",
            None,
            false,
        )
        .await?;
    let mut warnings = Vec::new();
    if !outcome.unknown_topics.is_empty() {
        warnings.push(format!(
            "{} unknown topic(s) were treated as domain",
            outcome.unknown_topics.len()
        ));
    }
    Ok(RebuildReport {
        mode,
        plan: lines,
        dry_run: false,
        applied_images: counts.images,
        applied_deletes: counts.deletes,
        resumed_skipped: counts.skipped_by_resume,
        skipped_domain_events: outcome.domain_skipped,
        last_position: Some(position_text(last)),
        new_generation: planned.new_generation,
        projectors: ran,
        invariants,
        warnings,
    })
}

/// The production [`FeedAdmin`] over the rebuilt database's backend.
pub struct PgFeedAdmin {
    backend: spot_postgres::PgBackend,
}

impl PgFeedAdmin {
    /// A feed admin over `backend`.
    #[must_use]
    pub fn new(backend: spot_postgres::PgBackend) -> Self {
        Self { backend }
    }
}

impl FeedAdmin for PgFeedAdmin {
    fn adopt<'a>(&'a self, generation: u64, seq: u64) -> Fut<'a, ()> {
        Box::pin(async move {
            spot_postgres::adopt_position(&self.backend, generation, seq)
                .await
                .map_err(|_| HostError::FeedFailed)
        })
    }

    fn open_generation<'a>(&'a self, generation: u64) -> Fut<'a, ()> {
        Box::pin(async move {
            spot_postgres::open_generation(&self.backend, generation)
                .await
                .map_err(|_| HostError::FeedFailed)
        })
    }
}
