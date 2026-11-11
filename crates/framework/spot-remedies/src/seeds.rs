//! The first generic remedies, seeded from the support-case corpus.
//!
//! Seven remedies cover the platform's own infrastructure, in generic terms (no ERP names): retry
//! and unstick jobs, flush a cache namespace, rebuild a retrieval index, repair edge drift, retry
//! quarantined items, and unstick a BPM case. Each ships a scenario the harness runs, a dry-run
//! plan, and an undo — or, for the one irreversible remedy, an explicit refusal. Every change goes
//! through the typed ports; none of them touches a raw storage handle.
//!
//! This module holds the job, cache, index and edge remedies and the helpers they share;
//! [`crate::seeds_more`] holds the quarantine, case and document remedies.

use serde_json::{Value as Json, json};
use spot_agent_tiers::Tier;

use crate::ctx::RemedyCtx;
use crate::error::RemedyError;
use crate::fixture::FakeWorld;
use crate::model::{
    Applied, Change, Check, ParamSchema, ParamType, Plan, PlanStep, RemedyId, RemedyMeta, Risk,
    UndoKind, UndoToken, Undone,
};
use crate::ports::{
    CacheChange, CacheNs, EdgeChange, EdgeRef, IndexChange, IndexHealth, IndexRef, JobChange,
    JobLife, JobRef, OpError,
};
use crate::remedy::Remedy;
use crate::scenario::{Scenario, TestSpec, UndoExpectation, WorldSpec};

pub use crate::seeds_more::RecomputeField;
use crate::seeds_more::{CaseUnstick, QuarantineRetryAll};

/// Every seed remedy, in a stable order.
#[must_use]
pub fn all() -> Vec<Box<dyn Remedy>> {
    vec![
        Box::new(JobRetryFailed),
        Box::new(JobUnstickRunning),
        Box::new(CacheFlushNamespace),
        Box::new(IndexRebuildRetrieval),
        Box::new(EdgeRepairDrift),
        Box::new(QuarantineRetryAll),
        Box::new(CaseUnstick),
    ]
}

/// Wraps a port failure as a remedy error.
pub(crate) fn op_err(error: OpError) -> RemedyError {
    RemedyError::OpFailed {
        operation: error.operation,
        detail: error.detail,
    }
}

/// A required string parameter.
pub(crate) fn str_param(id: &RemedyId, params: &Json, name: &str) -> Result<String, RemedyError> {
    params
        .get(name)
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RemedyError::ParamsInvalid {
            remedy: id.as_str().to_owned(),
            path: name.to_owned(),
            detail: "the parameter is required".to_owned(),
        })
}

/// The `job_id` parameter as a reference.
fn job_ref(id: &RemedyId, params: &Json) -> Result<JobRef, RemedyError> {
    Ok(JobRef::new(&str_param(id, params, "job_id")?))
}

/// A blocked precondition with the framework's stable code.
pub(crate) fn blocked(summary: &str) -> Check {
    Check::blocked("remedies.precondition_failed", summary)
}

/// The integer after a `prefix=` fingerprint (`entries=3`, `edges=7`).
pub(crate) fn fingerprint_int(text: &str, prefix: &str) -> Option<u64> {
    text.strip_prefix(prefix)?.parse::<u64>().ok()
}

/// Parses an index fingerprint (`stale:gen3`).
pub(crate) fn index_fingerprint(text: &str) -> Option<(IndexHealth, u64)> {
    let (health, generation) = text.split_once(":gen")?;
    Some((IndexHealth::parse(health)?, generation.parse::<u64>().ok()?))
}

/// The target reference after its `kind:` prefix (`job:17` -> `17`).
pub(crate) fn target_key<'a>(target: &'a str, kind: &str) -> Option<&'a str> {
    target.strip_prefix(kind)?.strip_prefix(':')
}

// ---------------------------------------------------------------------------------------------
// job.retry_failed
// ---------------------------------------------------------------------------------------------

/// Retries one failed background job.
struct JobRetryFailed;

impl Remedy for JobRetryFailed {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("job", ".retry_failed"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Retry a failed job".to_owned(),
            tier: Tier::T1,
            risk: Risk::Low,
            affects: Vec::new(),
            params: ParamSchema::empty().with_required("job_id", ParamType::String),
            preconditions: "The job exists and is failed.".to_owned(),
            undo: UndoKind::reversible("the job is set back to failed"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(id) = job_ref(&self.id(), params) else {
            return blocked("the job_id parameter is missing");
        };
        match ctx.jobs().job(&id) {
            Some(view) if view.life == JobLife::Failed => {
                Check::ready(&format!("job `{}` is failed", id.as_str()))
            }
            Some(view) => blocked(&format!(
                "job `{}` is {}, not failed",
                id.as_str(),
                view.life.as_str()
            )),
            None => blocked(&format!("no job `{}` exists", id.as_str())),
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let id = job_ref(&self.id(), params)?;
        let view = ctx.jobs().job(&id).ok_or_else(|| RemedyError::OpFailed {
            operation: "job_retry".to_owned(),
            detail: format!("no job `{}` exists", id.as_str()),
        })?;
        if view.life != JobLife::Failed {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("job `{}` is not failed", id.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("job:{}", id.as_str()),
            "job_retry",
            "retry the failed job",
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let id = job_ref(&self.id(), &plan.params)?;
        let now = ctx.now();
        let change: JobChange = ctx.jobs().retry(&id, now).map_err(op_err)?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::job(&id, change.before, change.after)],
            undo_token: UndoToken::derive(&plan.hash, now, 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        let change = applied
            .changes
            .first()
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "job_restore".to_owned(),
                detail: "the applied change is empty".to_owned(),
            })?;
        let key = target_key(&change.target, "job").ok_or_else(|| RemedyError::OpFailed {
            operation: "job_restore".to_owned(),
            detail: format!("`{}` is not a job target", change.target),
        })?;
        let life = JobLife::parse(&change.before).ok_or_else(|| RemedyError::OpFailed {
            operation: "job_restore".to_owned(),
            detail: format!("`{}` is not a job state", change.before),
        })?;
        ctx.jobs()
            .restore(&JobRef::new(key), life)
            .map_err(op_err)?;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a failed job is retried and then restored to failed",
            WorldSpec::new().with_job("j1", JobLife::Failed, 3, None),
            json!({"job_id": "j1"}),
            UndoExpectation::Restored,
            job_retry_applied,
            job_retry_undone,
        ))
    }
}

/// The invariant after retrying job `j1`: it is queued.
fn job_retry_applied(world: &FakeWorld) -> Result<(), String> {
    match world.job_life("j1") {
        Some(JobLife::Queued) => Ok(()),
        other => Err(format!("j1 is {other:?}, not queued")),
    }
}

/// The invariant after undoing: job `j1` is failed again.
fn job_retry_undone(world: &FakeWorld) -> Result<(), String> {
    match world.job_life("j1") {
        Some(JobLife::Failed) => Ok(()),
        other => Err(format!("j1 is {other:?}, not failed")),
    }
}

// ---------------------------------------------------------------------------------------------
// job.unstick_running
// ---------------------------------------------------------------------------------------------

/// Unsticks a running job whose lease has expired.
struct JobUnstickRunning;

impl Remedy for JobUnstickRunning {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("job", ".unstick_running"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Unstick a running job with an expired lease".to_owned(),
            tier: Tier::T1,
            risk: Risk::Low,
            affects: Vec::new(),
            params: ParamSchema::empty().with_required("job_id", ParamType::String),
            preconditions: "The job is running and its lease has expired.".to_owned(),
            undo: UndoKind::reversible("the job is set back to running"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(id) = job_ref(&self.id(), params) else {
            return blocked("the job_id parameter is missing");
        };
        let now = ctx.now();
        match ctx.jobs().job(&id) {
            Some(view) if view.life != JobLife::Running => blocked(&format!(
                "job `{}` is {}, not running",
                id.as_str(),
                view.life.as_str()
            )),
            Some(view) => match view.lease_expires_at {
                Some(expires) if expires <= now => Check::ready(&format!(
                    "job `{}` is running and its lease expired at {expires}",
                    id.as_str()
                )),
                _ => blocked(&format!(
                    "the lease on job `{}` has not expired",
                    id.as_str()
                )),
            },
            None => blocked(&format!("no job `{}` exists", id.as_str())),
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let id = job_ref(&self.id(), params)?;
        let now = ctx.now();
        let view = ctx.jobs().job(&id).ok_or_else(|| RemedyError::OpFailed {
            operation: "job_unstick".to_owned(),
            detail: format!("no job `{}` exists", id.as_str()),
        })?;
        let expired = view.life == JobLife::Running
            && matches!(view.lease_expires_at, Some(expires) if expires <= now);
        if !expired {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("job `{}` is not running with an expired lease", id.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("job:{}", id.as_str()),
            "job_unstick",
            "unstick the running job with an expired lease",
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let id = job_ref(&self.id(), &plan.params)?;
        let now = ctx.now();
        let change: JobChange = ctx.jobs().unstick(&id, now).map_err(op_err)?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::job(&id, change.before, change.after)],
            undo_token: UndoToken::derive(&plan.hash, now, 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        let change = applied
            .changes
            .first()
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "job_restore".to_owned(),
                detail: "the applied change is empty".to_owned(),
            })?;
        let key = target_key(&change.target, "job").ok_or_else(|| RemedyError::OpFailed {
            operation: "job_restore".to_owned(),
            detail: format!("`{}` is not a job target", change.target),
        })?;
        let life = JobLife::parse(&change.before).ok_or_else(|| RemedyError::OpFailed {
            operation: "job_restore".to_owned(),
            detail: format!("`{}` is not a job state", change.before),
        })?;
        ctx.jobs()
            .restore(&JobRef::new(key), life)
            .map_err(op_err)?;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a running job with an expired lease is unstuck and then restored",
            WorldSpec::new().with_job("j2", JobLife::Running, 1, Some(1_000)),
            json!({"job_id": "j2"}),
            UndoExpectation::Restored,
            job_unstick_applied,
            job_unstick_undone,
        ))
    }
}

/// The invariant after unsticking job `j2`: it is queued.
fn job_unstick_applied(world: &FakeWorld) -> Result<(), String> {
    match world.job_life("j2") {
        Some(JobLife::Queued) => Ok(()),
        other => Err(format!("j2 is {other:?}, not queued")),
    }
}

/// The invariant after undoing: job `j2` is running again.
fn job_unstick_undone(world: &FakeWorld) -> Result<(), String> {
    match world.job_life("j2") {
        Some(JobLife::Running) => Ok(()),
        other => Err(format!("j2 is {other:?}, not running")),
    }
}

// ---------------------------------------------------------------------------------------------
// cache.flush_namespace
// ---------------------------------------------------------------------------------------------

/// Flushes a cache namespace.
struct CacheFlushNamespace;

impl Remedy for CacheFlushNamespace {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("cache", ".flush_namespace"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Flush a cache namespace".to_owned(),
            tier: Tier::T1,
            risk: Risk::Low,
            affects: Vec::new(),
            params: ParamSchema::empty().with_required("namespace", ParamType::String),
            preconditions: "The namespace exists and holds entries.".to_owned(),
            undo: UndoKind::reversible("a cache is derived data; nothing durable is restored"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(ns) = str_param(&self.id(), params, "namespace") else {
            return blocked("the namespace parameter is missing");
        };
        let ns = CacheNs::new(&ns);
        if ctx.cache().entries(&ns) > 0 {
            Check::ready(&format!("namespace `{}` holds entries", ns.as_str()))
        } else {
            blocked(&format!("namespace `{}` holds no entries", ns.as_str()))
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let ns = CacheNs::new(&str_param(&self.id(), params, "namespace")?);
        let entries = ctx.cache().entries(&ns);
        if entries == 0 {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("namespace `{}` holds no entries", ns.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("cache:{}", ns.as_str()),
            "cache_flush",
            "flush the namespace",
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let ns = CacheNs::new(&str_param(&self.id(), &plan.params, "namespace")?);
        let change: CacheChange = ctx.cache().flush(&ns).map_err(op_err)?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::cache(&ns, change.before, change.after)],
            undo_token: UndoToken::derive(&plan.hash, ctx.now(), 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        let change = applied
            .changes
            .first()
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "cache_undo_flush".to_owned(),
                detail: "the applied change is empty".to_owned(),
            })?;
        let key = target_key(&change.target, "cache").ok_or_else(|| RemedyError::OpFailed {
            operation: "cache_undo_flush".to_owned(),
            detail: format!("`{}` is not a cache target", change.target),
        })?;
        let before = fingerprint_int(&change.before, "entries=").unwrap_or(0);
        let after = fingerprint_int(&change.after, "entries=").unwrap_or(0);
        let cache_change = CacheChange {
            ns: CacheNs::new(key),
            before: usize::try_from(before).unwrap_or(0),
            after: usize::try_from(after).unwrap_or(0),
        };
        ctx.cache().undo_flush(&cache_change).map_err(op_err)?;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a populated cache namespace is flushed and the reversal is recorded",
            WorldSpec::new().with_cache("sessions", 5),
            json!({"namespace": "sessions"}),
            UndoExpectation::Restored,
            cache_flush_applied,
            cache_flush_undone,
        ))
    }
}

/// The invariant after flushing: the namespace is empty.
fn cache_flush_applied(world: &FakeWorld) -> Result<(), String> {
    if world.cache_entries("sessions") == 0 {
        Ok(())
    } else {
        Err(format!(
            "sessions holds {} entries, not 0",
            world.cache_entries("sessions")
        ))
    }
}

/// The invariant after undoing: the cache is still empty (derived data, nothing durable changed).
fn cache_flush_undone(world: &FakeWorld) -> Result<(), String> {
    cache_flush_applied(world)
}

// ---------------------------------------------------------------------------------------------
// index.rebuild_retrieval
// ---------------------------------------------------------------------------------------------

/// Rebuilds a stale retrieval index.
struct IndexRebuildRetrieval;

impl Remedy for IndexRebuildRetrieval {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("index", ".rebuild_retrieval"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Rebuild a stale retrieval index".to_owned(),
            tier: Tier::T2,
            risk: Risk::Medium,
            affects: Vec::new(),
            params: ParamSchema::empty().with_required("index", ParamType::String),
            preconditions: "The index exists and is stale.".to_owned(),
            undo: UndoKind::reversible("the index is restored to its prior health and generation"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(name) = str_param(&self.id(), params, "index") else {
            return blocked("the index parameter is missing");
        };
        let name = IndexRef::new(&name);
        match ctx.index().index(&name) {
            Some(view) if view.health == IndexHealth::Stale => {
                Check::ready(&format!("index `{}` is stale", name.as_str()))
            }
            Some(view) => blocked(&format!(
                "index `{}` is {}, not stale",
                name.as_str(),
                view.health.as_str()
            )),
            None => blocked(&format!("no index `{}` exists", name.as_str())),
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let name = IndexRef::new(&str_param(&self.id(), params, "index")?);
        let view = ctx
            .index()
            .index(&name)
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "index_rebuild".to_owned(),
                detail: format!("no index `{}` exists", name.as_str()),
            })?;
        if view.health != IndexHealth::Stale {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("index `{}` is not stale", name.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("index:{}", name.as_str()),
            "index_rebuild",
            "rebuild the stale retrieval index",
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let name = IndexRef::new(&str_param(&self.id(), &plan.params, "index")?);
        let change: IndexChange = ctx.index().rebuild(&name).map_err(op_err)?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::index_pair(&name, &change)],
            undo_token: UndoToken::derive(&plan.hash, ctx.now(), 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        let change = applied
            .changes
            .first()
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "index_restore".to_owned(),
                detail: "the applied change is empty".to_owned(),
            })?;
        let key = target_key(&change.target, "index").ok_or_else(|| RemedyError::OpFailed {
            operation: "index_restore".to_owned(),
            detail: format!("`{}` is not an index target", change.target),
        })?;
        let (before, before_generation) =
            index_fingerprint(&change.before).ok_or_else(|| RemedyError::OpFailed {
                operation: "index_restore".to_owned(),
                detail: format!("`{}` is not an index fingerprint", change.before),
            })?;
        let (after, after_generation) =
            index_fingerprint(&change.after).ok_or_else(|| RemedyError::OpFailed {
                operation: "index_restore".to_owned(),
                detail: format!("`{}` is not an index fingerprint", change.after),
            })?;
        let index_change = IndexChange {
            name: IndexRef::new(key),
            before,
            after,
            before_generation,
            after_generation,
        };
        ctx.index().restore(&index_change).map_err(op_err)?;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a stale retrieval index is rebuilt and then restored",
            WorldSpec::new().with_index("retrieval", IndexHealth::Stale, 3),
            json!({"index": "retrieval"}),
            UndoExpectation::Restored,
            index_rebuild_applied,
            index_rebuild_undone,
        ))
    }
}

/// The invariant after rebuilding: the index is fresh.
fn index_rebuild_applied(world: &FakeWorld) -> Result<(), String> {
    match world.index_health("retrieval") {
        Some(IndexHealth::Fresh) => Ok(()),
        other => Err(format!("retrieval is {other:?}, not fresh")),
    }
}

/// The invariant after undoing: the index is stale again.
fn index_rebuild_undone(world: &FakeWorld) -> Result<(), String> {
    match world.index_health("retrieval") {
        Some(IndexHealth::Stale) => Ok(()),
        other => Err(format!("retrieval is {other:?}, not stale")),
    }
}

// ---------------------------------------------------------------------------------------------
// edge.repair_drift
// ---------------------------------------------------------------------------------------------

/// Repairs edge drift so the edge matches its source.
struct EdgeRepairDrift;

impl Remedy for EdgeRepairDrift {
    fn id(&self) -> RemedyId {
        RemedyId::new(concat!("edge", ".repair_drift"))
    }

    fn meta(&self) -> RemedyMeta {
        RemedyMeta {
            title: "Repair edge drift".to_owned(),
            tier: Tier::T2,
            risk: Risk::Medium,
            affects: Vec::new(),
            params: ParamSchema::empty().with_required("edge", ParamType::String),
            preconditions: "The edge exists and has drifted from its source.".to_owned(),
            undo: UndoKind::reversible("the edge is restored to its prior count"),
        }
    }

    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check {
        let Ok(edge) = str_param(&self.id(), params, "edge") else {
            return blocked("the edge parameter is missing");
        };
        let edge = EdgeRef::new(&edge);
        match ctx.edge().edge(&edge) {
            Some(view) if view.actual != view.expected => Check::ready(&format!(
                "edge `{}` has {} edges, expected {}",
                edge.as_str(),
                view.actual,
                view.expected
            )),
            Some(_) => blocked(&format!("edge `{}` has not drifted", edge.as_str())),
            None => blocked(&format!("no edge `{}` exists", edge.as_str())),
        }
    }

    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError> {
        let edge = EdgeRef::new(&str_param(&self.id(), params, "edge")?);
        let view = ctx
            .edge()
            .edge(&edge)
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "edge_repair".to_owned(),
                detail: format!("no edge `{}` exists", edge.as_str()),
            })?;
        if view.actual == view.expected {
            return Err(RemedyError::PreconditionFailed {
                remedy: self.id().as_str().to_owned(),
                detail: format!("edge `{}` has not drifted", edge.as_str()),
            });
        }
        let step = PlanStep::new(
            &format!("edge:{}", edge.as_str()),
            "edge_repair",
            "repair the edge drift through edge sync",
        );
        Ok(Plan::seal(self.id(), params.clone(), vec![step]))
    }

    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError> {
        let edge = EdgeRef::new(&str_param(&self.id(), &plan.params, "edge")?);
        let change: EdgeChange = ctx.edge().repair(&edge).map_err(op_err)?;
        Ok(Applied {
            remedy: self.id(),
            plan_hash: plan.hash.clone(),
            changes: vec![Change::edge(&edge, change.before, change.after)],
            undo_token: UndoToken::derive(&plan.hash, ctx.now(), 0),
        })
    }

    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError> {
        let change = applied
            .changes
            .first()
            .ok_or_else(|| RemedyError::OpFailed {
                operation: "edge_restore".to_owned(),
                detail: "the applied change is empty".to_owned(),
            })?;
        let key = target_key(&change.target, "edge").ok_or_else(|| RemedyError::OpFailed {
            operation: "edge_restore".to_owned(),
            detail: format!("`{}` is not an edge target", change.target),
        })?;
        let before =
            fingerprint_int(&change.before, "edges=").ok_or_else(|| RemedyError::OpFailed {
                operation: "edge_restore".to_owned(),
                detail: format!("`{}` is not an edge fingerprint", change.before),
            })?;
        let after =
            fingerprint_int(&change.after, "edges=").ok_or_else(|| RemedyError::OpFailed {
                operation: "edge_restore".to_owned(),
                detail: format!("`{}` is not an edge fingerprint", change.after),
            })?;
        let edge_change = EdgeChange {
            edge: EdgeRef::new(key),
            before,
            after,
        };
        ctx.edge().restore(&edge_change).map_err(op_err)?;
        Ok(Undone {
            remedy: self.id(),
            token: applied.undo_token.clone(),
            changes: applied.changes.clone(),
        })
    }

    fn self_test(&self) -> TestSpec {
        TestSpec::new(Scenario::new(
            "a drifted edge is repaired and then restored",
            WorldSpec::new().with_edge("customer-1", 10, 7),
            json!({"edge": "customer-1"}),
            UndoExpectation::Restored,
            edge_repair_applied,
            edge_repair_undone,
        ))
    }
}

/// The invariant after repair: the edge matches its source.
fn edge_repair_applied(world: &FakeWorld) -> Result<(), String> {
    match world.edge_actual("customer-1") {
        Some(10) => Ok(()),
        other => Err(format!("customer-1 has {other:?} edges, not 10")),
    }
}

/// The invariant after undoing: the edge is drifted again.
fn edge_repair_undone(world: &FakeWorld) -> Result<(), String> {
    match world.edge_actual("customer-1") {
        Some(7) => Ok(()),
        other => Err(format!("customer-1 has {other:?} edges, not 7")),
    }
}
