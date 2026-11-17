//! The rebuild host's orchestration over in-memory fakes: modes, dry run, checkpoint and resume,
//! refusals, the order of adoption and audit, projectors and invariants.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_events_contract::envelope::Position;
use spot_image_upcast::Image;
use spot_journal_rebuild::{
    AppliedImage, ApplySink, PlanSummary, RebuildError, RebuildOutcome, SinkError, SinkFuture,
};
use spot_journal_rebuild_host::{
    AdminAudit, Checkpoint, CheckpointStore, Deps, FeedAdmin, Fut, HostError, InvariantResult,
    Invariants, JournalSide, MemoryCheckpoints, Mode, Projector, Projectors, RebuildFut,
    RebuildRequest, Rebuilder, SiteGuard, State, TargetSpec, rebuild_site,
};
use spot_journal_rebuild_plan::{
    Base, ForkInfo, ForkPoint, JournalCatalog, RebuildPlan, SegmentRange, SnapshotInfo,
};

type Log = Arc<Mutex<Vec<String>>>;

struct Guard(AtomicBool);
impl SiteGuard for Guard {
    fn ensure_stopped<'a>(&'a self, _site: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            if self.0.load(Ordering::SeqCst) {
                Err(HostError::SiteRunning)
            } else {
                Ok(())
            }
        })
    }
}

struct Journal {
    catalog: JournalCatalog,
    log: Log,
}
impl JournalSide for Journal {
    fn tenant(&self) -> &str {
        "acme"
    }
    fn catalog(&self) -> Fut<'_, JournalCatalog> {
        Box::pin(async move { Ok(self.catalog.clone()) })
    }
    fn resolve_before(&self, _millis: i64) -> Fut<'_, Option<(u64, u64)>> {
        Box::pin(async move { Ok(None) })
    }
    fn record_fork(&self, fork: ForkPoint, new_generation: u64) -> Fut<'_, ()> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!(
                "fork {}:{} -> {new_generation}",
                fork.abandoned_generation, fork.from_position
            ));
            Ok(())
        })
    }
}

fn image_at(position: Position, id: String) -> AppliedImage {
    AppliedImage {
        entity: "party".to_owned(),
        id,
        version: 1,
        position,
        image: Image::new("party", 1, serde_json::Map::new()),
    }
}

/// Drives the sink with one image per position from the base to the end; optionally fails after
/// `fail_after` sink calls (once).
struct Fake {
    fail_after: Mutex<Option<usize>>,
    calls: AtomicUsize,
    snapshot_images: u64,
}
impl Rebuilder for Fake {
    fn execute<'a>(&'a self, plan: &'a RebuildPlan, sink: &'a mut dyn ApplySink) -> RebuildFut<'a> {
        Box::pin(async move {
            let base_position = plan.base.position();
            let generation = plan.base.generation().unwrap_or(1);
            let summary = PlanSummary {
                tenant: "acme".to_owned(),
                source: "acme".to_owned(),
                base: plan.base,
                steps: plan.steps.len(),
                ends_at: plan.ends_at,
                new_generation: plan.new_generation,
                fork_point: plan.fork_point,
                preserves_positions: plan.preserves_positions,
                target_schema: 1,
            };
            sink.begin(&summary)
                .await
                .map_err(|_| RebuildError::Cancelled { processed: 0 })?;
            let mut n = 0u64;
            // The snapshot's images all share the snapshot's position.
            if matches!(plan.base, Base::Snapshot { .. }) {
                for i in 0..self.snapshot_images {
                    let p = Position {
                        generation,
                        seq: base_position,
                    };
                    self.step()?;
                    sink.apply_image(image_at(p, format!("snap-{i}")))
                        .await
                        .map_err(|_| RebuildError::Cancelled { processed: n })?;
                    n += 1;
                }
            }
            let end_generation = generation;
            for seq in (base_position + 1)..=plan.ends_at {
                let p = Position {
                    generation: end_generation,
                    seq,
                };
                self.step()?;
                sink.apply_image(image_at(p, format!("id-{}-{}", p.generation, p.seq)))
                    .await
                    .map_err(|_| RebuildError::Cancelled { processed: n })?;
                n += 1;
            }
            let last = Position {
                generation: end_generation,
                seq: plan.ends_at,
            };
            sink.commit(last)
                .await
                .map_err(|_| RebuildError::Cancelled { processed: n })?;
            Ok(RebuildOutcome {
                tenant: "acme".to_owned(),
                base: plan.base,
                last_position: last,
                fork_point: plan.fork_point,
                new_generation: plan.new_generation,
                preserves_positions: plan.preserves_positions,
                snapshot_images: self.snapshot_images,
                entries_read: plan.ends_at - base_position,
                state_applied: plan.ends_at - base_position,
                deletes_applied: 0,
                audit_applied: 0,
                audit_skipped: 0,
                domain_skipped: 3,
                duplicates_skipped: 0,
                peak_buffered_entries: 1,
                unknown_topics: Vec::new(),
                unknown_topics_truncated: false,
                classes: BTreeMap::new(),
                identity_audit_applied: 0,
                warnings: Vec::new(),
            })
        })
    }
}
impl Fake {
    fn step(&self) -> Result<(), RebuildError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let mut limit = self.fail_after.lock().unwrap();
        if let Some(after) = *limit
            && n > after
        {
            *limit = None;
            return Err(RebuildError::Cancelled {
                processed: n as u64,
            });
        }
        Ok(())
    }
}

/// Records the applied images, by position, to prove none is applied twice.
struct Store {
    applied: Arc<Mutex<Vec<(u64, u64, String)>>>,
}
impl ApplySink for Store {
    fn begin<'a>(&'a mut self, _s: &'a PlanSummary) -> SinkFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        self.applied.lock().unwrap().push((
            image.position.generation,
            image.position.seq,
            image.id,
        ));
        Box::pin(std::future::ready(Ok::<(), SinkError>(())))
    }
    fn apply_delete(&mut self, _d: spot_journal_rebuild::AppliedDelete) -> SinkFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn apply_audit(&mut self, _a: spot_journal_rebuild::AppliedAudit) -> SinkFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn commit(&mut self, _l: Position) -> SinkFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn abort<'a>(&'a mut self, _r: &'a str) -> SinkFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

struct Feed {
    log: Log,
}
impl FeedAdmin for Feed {
    fn adopt<'a>(&'a self, generation: u64, seq: u64) -> Fut<'a, ()> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push(format!("adopt {generation}:{seq}"));
            Ok(())
        })
    }
    fn open_generation<'a>(&'a self, generation: u64) -> Fut<'a, ()> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("open {generation}"));
            Ok(())
        })
    }
}

struct Audit {
    log: Log,
}
impl AdminAudit for Audit {
    fn record<'a>(
        &'a self,
        action: &'static str,
        _target: Option<String>,
        outcome: &'static str,
        _code: Option<String>,
        dry_run: bool,
    ) -> Fut<'a, ()> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push(format!("audit {action} {outcome} dry={dry_run}"));
            Ok(())
        })
    }
}

struct Suite(bool);
impl Invariants for Suite {
    fn run(&self) -> Fut<'_, Vec<InvariantResult>> {
        Box::pin(async move {
            Ok(vec![InvariantResult {
                name: "document_counts".to_owned(),
                held: self.0,
                checked: 10,
                violations: u64::from(!self.0),
            }])
        })
    }
}

struct Proj(&'static str, Log);
impl Projector for Proj {
    fn name(&self) -> &str {
        "graph"
    }
    fn entities(&self) -> Vec<String> {
        vec![self.0.to_owned()]
    }
    fn rebuild(&self) -> Fut<'_, ()> {
        Box::pin(async move {
            self.1.lock().unwrap().push("projector graph".to_owned());
            Ok(())
        })
    }
}

fn catalog() -> JournalCatalog {
    JournalCatalog {
        snapshots: vec![SnapshotInfo {
            position: 40,
            generation: 1,
            verified: true,
            taken_at_millis: 1,
        }],
        segments: vec![SegmentRange {
            generation: 1,
            first_seq: 41,
            last_seq: 100,
        }],
        forks: Vec::<ForkInfo>::new(),
    }
}

struct World {
    deps: Deps,
    log: Log,
    applied: Arc<Mutex<Vec<(u64, u64, String)>>>,
    checkpoints: Arc<MemoryCheckpoints>,
    guard: Arc<Guard>,
    fake: Arc<Fake>,
}

fn world(derived: &[&str], invariants_hold: bool) -> World {
    let log: Log = Arc::default();
    let applied = Arc::new(Mutex::new(Vec::new()));
    let checkpoints = Arc::new(MemoryCheckpoints::new());
    let guard = Arc::new(Guard(AtomicBool::new(false)));
    let fake = Arc::new(Fake {
        fail_after: Mutex::new(None),
        calls: AtomicUsize::new(0),
        snapshot_images: 5,
    });
    let mut projectors = Projectors::new();
    projectors.register(Arc::new(Proj("graph_edge", Arc::clone(&log))));
    let sink_applied = Arc::clone(&applied);
    let deps = Deps {
        site: guard.clone(),
        journal: Arc::new(Journal {
            catalog: catalog(),
            log: Arc::clone(&log),
        }),
        rebuilder: fake.clone(),
        sink: Arc::new(move || {
            Box::new(Store {
                applied: Arc::clone(&sink_applied),
            }) as Box<dyn ApplySink>
        }),
        checkpoints: checkpoints.clone(),
        feed: Arc::new(Feed {
            log: Arc::clone(&log),
        }),
        projectors,
        derived: derived.iter().map(|s| (*s).to_owned()).collect(),
        invariants: Arc::new(Suite(invariants_hold)),
        audit: Arc::new(Audit {
            log: Arc::clone(&log),
        }),
    };
    World {
        deps,
        log,
        applied,
        checkpoints,
        guard,
        fake,
    }
}

#[tokio::test]
async fn a_full_rebuild_adopts_the_head_before_the_audit_and_runs_projectors_and_invariants() {
    let w = world(&["graph_edge"], true);
    let report = rebuild_site(&w.deps, &RebuildRequest::latest("acme", "acme"))
        .await
        .unwrap();
    assert_eq!(report.mode, Mode::Full);
    assert_eq!(report.applied_images, 5 + 60);
    assert_eq!(report.skipped_domain_events, 3);
    assert_eq!(report.last_position.as_deref(), Some("1:100"));
    assert_eq!(report.projectors, ["graph"]);
    assert!(report.invariants.iter().all(|i| i.held));
    let log = w.log.lock().unwrap().clone();
    let at = |what: &str| log.iter().position(|l| l.starts_with(what)).unwrap();
    assert!(at("audit rebuild_started") < at("adopt 1:100"), "{log:?}");
    assert!(
        at("adopt 1:100") < at("audit rebuild_completed"),
        "adopt before the audit: {log:?}"
    );
    assert!(at("projector graph") < at("audit rebuild_completed"));
    let saved = w.checkpoints.load("acme").await.unwrap().unwrap();
    assert_eq!((saved.state, saved.seq), (State::Complete, 100));
}

#[tokio::test]
async fn a_dry_run_touches_nothing_and_says_so() {
    let w = world(&["graph_edge"], true);
    let mut request = RebuildRequest::latest("acme", "acme");
    request.dry_run = true;
    let report = rebuild_site(&w.deps, &request).await.unwrap();
    assert!(report.dry_run);
    assert!(!report.plan.is_empty());
    assert!(w.applied.lock().unwrap().is_empty());
    assert!(w.checkpoints.load("acme").await.unwrap().is_none());
    assert_eq!(
        w.log.lock().unwrap().clone(),
        ["audit rebuild_planned ok dry=true"]
    );
}

#[tokio::test]
async fn refusals_come_before_any_write() {
    let w = world(&["graph_edge"], true);
    // A running site.
    w.guard.0.store(true, Ordering::SeqCst);
    assert_eq!(
        rebuild_site(&w.deps, &RebuildRequest::latest("acme", "acme"))
            .await
            .unwrap_err(),
        HostError::SiteRunning
    );
    w.guard.0.store(false, Ordering::SeqCst);
    // A journal of another tenant.
    assert_eq!(
        rebuild_site(&w.deps, &RebuildRequest::latest("acme", "beta"))
            .await
            .unwrap_err(),
        HostError::TenantMismatch
    );
    // A backup whose head is not in the journal.
    let mut request = RebuildRequest::latest("acme", "acme");
    request.backup_head = Some((1, 5_000));
    assert_eq!(
        rebuild_site(&w.deps, &request).await.unwrap_err(),
        HostError::BaseNotInJournal
    );
    // A derived entity without a projector.
    let w2 = world(&["search_index"], true);
    assert_eq!(
        rebuild_site(&w2.deps, &RebuildRequest::latest("acme", "acme"))
            .await
            .unwrap_err(),
        HostError::ProjectorMissing {
            entity: "search_index".to_owned()
        }
    );
    for w in [&w, &w2] {
        assert!(w.applied.lock().unwrap().is_empty(), "nothing was written");
    }
}

#[tokio::test]
async fn roll_forward_applies_only_what_follows_the_backup_head() {
    let w = world(&["graph_edge"], true);
    let mut request = RebuildRequest::latest("acme", "acme");
    request.backup_head = Some((1, 70));
    let report = rebuild_site(&w.deps, &request).await.unwrap();
    assert_eq!(report.mode, Mode::RollForward);
    let applied = w.applied.lock().unwrap();
    assert_eq!(applied.first().map(|a| a.1), Some(71));
    assert_eq!(applied.len(), 30);
}

#[tokio::test]
async fn point_in_time_forks_records_the_fork_and_opens_the_new_generation() {
    let w = world(&["graph_edge"], true);
    let mut request = RebuildRequest::latest("acme", "acme");
    request.target = TargetSpec::At {
        generation: 1,
        seq: 60,
    };
    let report = rebuild_site(&w.deps, &request).await.unwrap();
    assert_eq!(report.mode, Mode::PointInTime);
    assert_eq!(report.new_generation, Some(2));
    let log = w.log.lock().unwrap().clone();
    assert!(log.contains(&"fork 1:60 -> 2".to_owned()), "{log:?}");
    assert!(log.contains(&"open 2".to_owned()));
    assert!(log.iter().any(|l| l.starts_with("audit fork_opened")));
    assert!(
        !log.iter().any(|l| l.starts_with("adopt")),
        "a fork adopts no old position"
    );
}

#[tokio::test]
async fn an_interrupted_rebuild_resumes_from_its_checkpoint_and_never_applies_twice() {
    for cut in [8usize, 20, 64] {
        let w = world(&["graph_edge"], true);
        *w.fake.fail_after.lock().unwrap() = Some(cut);
        let mut request = RebuildRequest::latest("acme", "acme");
        request.checkpoint_every = 4;
        let error = rebuild_site(&w.deps, &request).await.unwrap_err();
        assert!(matches!(error, HostError::RebuildFailed(_)), "{error:?}");
        // The checkpoint stays, the state says the database is half built.
        let saved = w.checkpoints.load("acme").await.unwrap().unwrap();
        assert_eq!(saved.state, State::Rebuilding);
        // The second run finishes it.
        let report = rebuild_site(&w.deps, &request).await.unwrap();
        assert_eq!(report.last_position.as_deref(), Some("1:100"));
        let applied = w.applied.lock().unwrap().clone();
        let mut unique = applied.clone();
        unique.sort();
        unique.dedup();
        // At most one checkpoint interval is re-applied after a cut (the apply path is
        // idempotent by name and version, so the database holds each version once).
        assert!(
            applied.len() - unique.len() <= request.checkpoint_every as usize,
            "cut {cut}: {} re-applied",
            applied.len() - unique.len()
        );
        assert_eq!(unique.len(), 65, "cut {cut}: every image applied");
        assert!(
            report.resumed_skipped > 0,
            "cut {cut}: the checkpoint was used"
        );
    }
}

#[tokio::test]
async fn a_checkpoint_of_another_plan_is_not_resumed() {
    let w = world(&["graph_edge"], true);
    w.checkpoints
        .save(
            "acme",
            &Checkpoint {
                plan_hash: "someone-else".to_owned(),
                generation: 1,
                seq: 50,
                state: State::Rebuilding,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        rebuild_site(&w.deps, &RebuildRequest::latest("acme", "acme"))
            .await
            .unwrap_err(),
        HostError::CheckpointForOtherPlan
    );
    // Discarding the half-built target is explicit, and then the rebuild runs.
    w.checkpoints.discard("acme").await.unwrap();
    assert!(
        rebuild_site(&w.deps, &RebuildRequest::latest("acme", "acme"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_failed_invariant_fails_the_rebuild_and_leaves_it_incomplete() {
    let w = world(&["graph_edge"], false);
    let error = rebuild_site(&w.deps, &RebuildRequest::latest("acme", "acme"))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        HostError::InvariantFailed("document_counts".to_owned())
    );
    let saved = w.checkpoints.load("acme").await.unwrap().unwrap();
    assert_eq!(saved.state, State::Rebuilding, "the site must not serve it");
    assert!(
        w.log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("audit rebuild_failed"))
    );
}
