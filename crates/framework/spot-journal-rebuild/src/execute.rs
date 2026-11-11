//! The executor: checks first, applies second.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use spot_events_contract::envelope::Position;
use spot_image_upcast::{Image, Registry, UpcastError};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_rebuild_plan::{Base, RebuildPlan};
use spot_journal_snapshot::{SnapshotError, SnapshotKeys, SnapshotReader};
use spot_journal_store::JournalStore;
use spot_journal_verify::{Expected, Mode, verify_tenant};

use crate::entry::{Entry, Op};
use crate::error::RebuildError;
use crate::options::{EventClass, Progress, RebuildOptions, RebuildOutcome};
use crate::sink::{AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError};
use crate::stream::{Feed, Manifests, position};

/// The keys a rebuild trusts: the tenant's public journal keys for the manifests, and for the
/// snapshot index.
pub struct RebuildKeys<'a> {
    /// Verifies the signed manifests.
    pub manifest: &'a VerifyingKeys,
    /// Verifies the snapshot index.
    pub snapshot: &'a dyn SnapshotKeys,
}

/// What the executor remembers about a record it has applied.
#[derive(Clone, Copy)]
struct RecordState {
    version: u64,
    deleted: bool,
}

enum Decision {
    Apply,
    Duplicate,
    Regressed { applied: u64 },
}

/// Whether an entry for a record in state `previous` is applied, skipped as a re-delivery of the
/// same version, or refused as a regression.
fn decide(previous: Option<RecordState>, op: Op, version: u64) -> Decision {
    let Some(previous) = previous else {
        return Decision::Apply;
    };
    let applied = previous.version;
    if op == Op::Insert && previous.deleted {
        // A record deleted and inserted again starts a new life: its version does not have to
        // exceed the old one.
        return Decision::Apply;
    }
    if version > applied {
        return Decision::Apply;
    }
    if version < applied {
        return Decision::Regressed { applied };
    }
    // The same version again: a re-delivery, except a delete of a live record, which carries
    // the version the record last had.
    if op == Op::Delete && !previous.deleted {
        Decision::Apply
    } else {
        Decision::Duplicate
    }
}

fn snapshot_failed(error: &SnapshotError) -> RebuildError {
    RebuildError::SnapshotFailed { code: error.code() }
}

fn sink_failed(error: SinkError) -> RebuildError {
    RebuildError::SinkFailed {
        reason: error.reason,
    }
}

/// The consistency of the plan with itself: the parts a reader of the plan alone can check. Gaps
/// between the base and the first step, and between steps, are position gaps found while reading.
fn check_plan(plan: &RebuildPlan) -> Result<(), RebuildError> {
    let mut previous_last = 0_u64;
    for (index, step) in plan.steps.iter().enumerate() {
        if step.first_seq() == 0 || step.first_seq() > step.last_seq() {
            return Err(RebuildError::PlanInvalid("a step holds no position"));
        }
        if index > 0 && step.first_seq() <= previous_last {
            return Err(RebuildError::PlanInvalid("steps are not in position order"));
        }
        previous_last = step.last_seq();
    }
    let last = plan
        .steps
        .last()
        .map_or(plan.base.position(), |s| s.last_seq());
    if last != plan.ends_at {
        return Err(RebuildError::PlanInvalid(
            "the plan does not end at its last step",
        ));
    }
    if plan.fork_point.is_some() != plan.new_generation.is_some() {
        return Err(RebuildError::PlanInvalid(
            "a fork point and a new generation go together",
        ));
    }
    if let Some(fork) = plan.fork_point
        && fork.from_position != plan.ends_at
    {
        return Err(RebuildError::PlanInvalid(
            "the fork point is not where the plan ends",
        ));
    }
    if plan.fork_point.is_some() == plan.preserves_positions {
        return Err(RebuildError::PlanInvalid(
            "a plan preserves positions exactly when it does not fork",
        ));
    }
    if matches!(plan.base, Base::Empty) && plan.steps.is_empty() {
        return Err(RebuildError::PlanInvalid("an empty base needs steps"));
    }
    Ok(())
}

/// The generation the committed position belongs to.
fn last_generation(plan: &RebuildPlan) -> Result<u64, RebuildError> {
    plan.steps
        .last()
        .map(spot_journal_rebuild_plan::Step::generation)
        .or_else(|| plan.base.generation())
        .or_else(|| plan.fork_point.map(|f| f.abandoned_generation))
        .ok_or(RebuildError::PlanInvalid(
            "no generation for a plan without steps",
        ))
}

/// Runs `plan` for `options.tenant` and hands the result to `sink`.
///
/// Order of work:
///
/// 1. the options and the plan are checked;
/// 2. unless `options.skip_verify`, a light verification of every generation the plan reads
///    (any tampering finding stops the rebuild), and the manifests are loaded and verified;
/// 3. the base snapshot index is opened and the segments are read once (`options.prescan`), so
///    a missing upcaster step, a gap, a foreign or malformed entry is found **before anything is
///    applied**;
/// 4. `sink.begin`, then the snapshot images, then each step's entries in position order, state
///    images upcast to the target schema, and `sink.commit` with the last position.
///
/// A failure after `begin` calls `sink.abort` with the error's code and returns the error. The
/// executor never writes to the journal: a fork is reported in the outcome and recorded by the
/// relay.
///
/// # Errors
/// [`RebuildError`]; see its variants.
pub async fn execute(
    store: &dyn JournalStore,
    keys: &RebuildKeys<'_>,
    registry: &Registry,
    plan: &RebuildPlan,
    sink: &mut dyn ApplySink,
    options: &RebuildOptions<'_>,
) -> Result<RebuildOutcome, RebuildError> {
    options.check()?;
    check_plan(plan)?;
    let last = position(last_generation(plan)?, plan.ends_at);

    let mut generations: BTreeSet<u64> = plan.steps.iter().map(|s| s.generation()).collect();
    generations.extend(plan.base.generation());
    let generations: Vec<u64> = generations.into_iter().collect();

    if !options.skip_verify {
        verify_light(store, keys, &generations, options).await?;
    }
    let manifests = Manifests::load(
        store,
        keys.manifest,
        &options.tenant,
        &options.source,
        &generations,
    )
    .await?;

    let snapshot = match plan.base {
        Base::Snapshot {
            position: at,
            generation,
        } => {
            let reader = SnapshotReader::open(
                store,
                options.tenant.as_str(),
                generation,
                at,
                keys.snapshot,
            )
            .await
            .map_err(|e| snapshot_failed(&e))?;
            check_snapshot_schemas(&reader, registry, options)?;
            Some((reader, position(generation, at)))
        }
        Base::Backup { .. } | Base::Empty => None,
    };

    let first_seq = plan.base.position().saturating_add(1);
    if options.prescan {
        prescan(store, &manifests, plan, first_seq, registry, options).await?;
    }

    let summary = PlanSummary {
        tenant: options.tenant.as_str().to_owned(),
        source: options.source.clone(),
        base: plan.base,
        steps: plan.steps.len(),
        ends_at: plan.ends_at,
        new_generation: plan.new_generation,
        fork_point: plan.fork_point,
        preserves_positions: plan.preserves_positions,
        target_schema: options.target_schema,
    };
    sink.begin(&summary).await.map_err(sink_failed)?;

    let mut run = Run {
        registry,
        options,
        counters: Counters::default(),
        records: BTreeMap::new(),
        processed: 0,
    };
    let applied = run
        .apply_all(store, &manifests, plan, first_seq, snapshot, sink)
        .await;
    let peak = match applied {
        Ok(peak) => peak,
        Err(error) => {
            // The sink is told why; an abort that itself fails changes nothing for the caller.
            let _ = sink.abort(error.code()).await;
            return Err(error);
        }
    };
    if let Err(error) = sink.commit(last).await {
        let error = sink_failed(error);
        let _ = sink.abort(error.code()).await;
        return Err(error);
    }
    let c = run.counters;
    Ok(RebuildOutcome {
        tenant: summary.tenant,
        base: plan.base,
        last_position: last,
        fork_point: plan.fork_point,
        new_generation: plan.new_generation,
        preserves_positions: plan.preserves_positions,
        snapshot_images: c.snapshot_images,
        entries_read: c.entries_read,
        state_applied: c.state_applied,
        deletes_applied: c.deletes_applied,
        audit_applied: c.audit_applied,
        audit_skipped: c.audit_skipped,
        domain_skipped: c.domain_skipped,
        duplicates_skipped: c.duplicates_skipped,
        peak_buffered_entries: peak,
    })
}

async fn verify_light(
    store: &dyn JournalStore,
    keys: &RebuildKeys<'_>,
    generations: &[u64],
    options: &RebuildOptions<'_>,
) -> Result<(), RebuildError> {
    for &generation in generations {
        let expected = Expected {
            tenant: options.tenant.clone(),
            source: options.source.clone(),
            generation: Some(generation),
        };
        let report = verify_tenant(store, keys.manifest, &expected, Mode::Light)
            .await
            .map_err(|e| RebuildError::VerifyUnavailable { code: e.code() })?;
        let tampering: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.is_tampering())
            .collect();
        if let Some(first) = tampering.first() {
            return Err(RebuildError::VerifyFailed {
                kind: first.kind.code(),
                generation: first.generation,
                first_seq: first.first_seq,
                last_seq: first.last_seq,
                count: tampering.len(),
            });
        }
    }
    Ok(())
}

/// Refuses an entity schema the registry cannot carry to the target.
fn check_schema(
    registry: &Registry,
    entity: &str,
    schema: u64,
    target: u64,
) -> Result<(), RebuildError> {
    if schema > target {
        return Err(RebuildError::UpcastFailed {
            entity: entity.to_owned(),
            code: UpcastError::FromTheFuture {
                entity: String::new(),
                schema,
                target,
            }
            .code(),
        });
    }
    match registry.missing_step(entity, schema, target) {
        Some(from) => Err(RebuildError::MissingUpcast {
            entity: entity.to_owned(),
            from,
            to: target,
        }),
        None => Ok(()),
    }
}

fn check_snapshot_schemas(
    reader: &SnapshotReader<'_>,
    registry: &Registry,
    options: &RebuildOptions<'_>,
) -> Result<(), RebuildError> {
    for (entity, schemas) in &reader.index().body.entity_schemas {
        for &schema in schemas {
            check_schema(registry, entity, schema, options.target_for(entity))?;
        }
    }
    Ok(())
}

/// Reads the planned segments once, applying nothing.
async fn prescan(
    store: &dyn JournalStore,
    manifests: &Manifests,
    plan: &RebuildPlan,
    first_seq: u64,
    registry: &Registry,
    options: &RebuildOptions<'_>,
) -> Result<(), RebuildError> {
    let mut seen: BTreeSet<(String, u64)> = BTreeSet::new();
    let mut feed = Feed::new(
        store,
        &options.tenant,
        &options.source,
        manifests,
        &plan.steps,
        first_seq,
    );
    while let Some(batch) = feed.next_batch().await? {
        for entry in batch {
            if let Some(state) = entry.state
                && state.op != Op::Delete
                && seen.insert((entry.entity.clone(), state.entity_schema))
            {
                check_schema(
                    registry,
                    &entry.entity,
                    state.entity_schema,
                    options.target_for(&entry.entity),
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Counters {
    snapshot_images: u64,
    entries_read: u64,
    state_applied: u64,
    deletes_applied: u64,
    audit_applied: u64,
    audit_skipped: u64,
    domain_skipped: u64,
    duplicates_skipped: u64,
}

struct Run<'r, 'o> {
    registry: &'r Registry,
    options: &'r RebuildOptions<'o>,
    counters: Counters,
    records: BTreeMap<(String, String), RecordState>,
    processed: u64,
}

impl Run<'_, '_> {
    /// Notes one more item handed over or skipped: checks the cancel flag first, reports
    /// progress after.
    fn tick(&mut self, at: Position) -> Result<(), RebuildError> {
        if self
            .options
            .cancel
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(RebuildError::Cancelled {
                processed: self.processed,
            });
        }
        self.processed += 1;
        if let Some(report) = self.options.progress
            && self.processed.is_multiple_of(self.options.progress_every)
        {
            report(Progress {
                processed: self.processed,
                position: at,
            });
        }
        Ok(())
    }

    async fn apply_all(
        &mut self,
        store: &dyn JournalStore,
        manifests: &Manifests,
        plan: &RebuildPlan,
        first_seq: u64,
        snapshot: Option<(SnapshotReader<'_>, Position)>,
        sink: &mut dyn ApplySink,
    ) -> Result<usize, RebuildError> {
        if let Some((reader, at)) = snapshot {
            self.apply_snapshot(&reader, at, sink).await?;
        }
        let mut feed = Feed::new(
            store,
            &self.options.tenant,
            &self.options.source,
            manifests,
            &plan.steps,
            first_seq,
        );
        while let Some(batch) = feed.next_batch().await? {
            for entry in batch {
                self.apply_entry(entry, sink).await?;
            }
        }
        Ok(feed.peak())
    }

    async fn apply_snapshot(
        &mut self,
        reader: &SnapshotReader<'_>,
        at: Position,
        sink: &mut dyn ApplySink,
    ) -> Result<(), RebuildError> {
        let mut images = reader.images();
        while let Some(record) = images.next().await.map_err(|e| snapshot_failed(&e))? {
            self.tick(at)?;
            let target = self.options.target_for(&record.entity);
            let image =
                Image::from_parts(record.entity.clone(), record.entity_schema, record.image)
                    .map_err(|e| upcast_failed(&record.entity, &e))?;
            let image = self
                .registry
                .upcast(image, target)
                .map_err(|e| upcast_failed(&record.entity, &e))?;
            self.records.insert(
                (record.entity.clone(), record.id.clone()),
                RecordState {
                    version: record.version,
                    deleted: false,
                },
            );
            sink.apply_image(AppliedImage {
                entity: record.entity,
                id: record.id,
                version: record.version,
                position: at,
                image,
            })
            .await
            .map_err(sink_failed)?;
            self.counters.snapshot_images += 1;
        }
        Ok(())
    }

    async fn apply_entry(
        &mut self,
        entry: Entry,
        sink: &mut dyn ApplySink,
    ) -> Result<(), RebuildError> {
        self.tick(entry.position)?;
        self.counters.entries_read += 1;
        match entry.class {
            EventClass::Domain => {
                self.counters.domain_skipped += 1;
                Ok(())
            }
            EventClass::Audit => {
                if !self.options.replay_audit {
                    self.counters.audit_skipped += 1;
                    return Ok(());
                }
                sink.apply_audit(AppliedAudit {
                    event_id: entry.event_id,
                    topic: entry.topic,
                    position: entry.position,
                    entity: entry.entity,
                    id: entry.id,
                    data: entry.data,
                })
                .await
                .map_err(sink_failed)?;
                self.counters.audit_applied += 1;
                Ok(())
            }
            EventClass::State => self.apply_state(entry, sink).await,
        }
    }

    async fn apply_state(
        &mut self,
        entry: Entry,
        sink: &mut dyn ApplySink,
    ) -> Result<(), RebuildError> {
        let Some(state) = entry.state else {
            return Err(RebuildError::EntryInvalid {
                generation: entry.position.generation,
                seq: entry.position.seq,
                reason: "a state entry has no state",
            });
        };
        let key = (entry.entity.clone(), entry.id.clone());
        match decide(self.records.get(&key).copied(), state.op, state.version) {
            Decision::Duplicate => {
                self.counters.duplicates_skipped += 1;
                return Ok(());
            }
            Decision::Regressed { applied } => {
                return Err(RebuildError::VersionRegressed {
                    entity: entry.entity,
                    id: entry.id,
                    generation: entry.position.generation,
                    seq: entry.position.seq,
                    applied,
                    found: state.version,
                });
            }
            Decision::Apply => {}
        }
        if state.op == Op::Delete {
            sink.apply_delete(AppliedDelete {
                entity: entry.entity,
                id: entry.id,
                version: state.version,
                position: entry.position,
            })
            .await
            .map_err(sink_failed)?;
            self.records.insert(
                key,
                RecordState {
                    version: state.version,
                    deleted: true,
                },
            );
            self.counters.deletes_applied += 1;
            return Ok(());
        }
        let target = self.options.target_for(&entry.entity);
        let image = Image::from_parts(entry.entity.clone(), state.entity_schema, state.image)
            .map_err(|e| upcast_failed(&entry.entity, &e))?;
        let image = self
            .registry
            .upcast(image, target)
            .map_err(|e| upcast_failed(&entry.entity, &e))?;
        sink.apply_image(AppliedImage {
            entity: entry.entity,
            id: entry.id,
            version: state.version,
            position: entry.position,
            image,
        })
        .await
        .map_err(sink_failed)?;
        self.records.insert(
            key,
            RecordState {
                version: state.version,
                deleted: false,
            },
        );
        self.counters.state_applied += 1;
        Ok(())
    }
}

fn upcast_failed(entity: &str, error: &UpcastError) -> RebuildError {
    match error {
        UpcastError::NoPath { from, to, .. } => RebuildError::MissingUpcast {
            entity: entity.to_owned(),
            from: *from,
            to: *to,
        },
        other => RebuildError::UpcastFailed {
            entity: entity.to_owned(),
            code: other.code(),
        },
    }
}
