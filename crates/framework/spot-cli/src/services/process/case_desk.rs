//! The BPM service's [`CaseDesk`] (WP-AI-07.T10): the stuck cases of the site, where one case
//! stands, and the operator's unstick, as the port the remedy world's cases part
//! (`world_access/cases.rs`) sits on. With it, the `case.unstick` remedy runs on a live site.
//!
//! # What "stuck" means (one rule, one function)
//!
//! [`standing`] is the only place the rule lives; the unit tests pin it. It is built on what
//! `process_insight` already means by a stuck run (an open attempt that has stayed in its state
//! longer than its node expects, with a reason code), narrowed to the reasons the **platform** can
//! clear:
//!
//! | insight reason       | the desk says | why                                                      |
//! |----------------------|---------------|----------------------------------------------------------|
//! | `running`            | **stuck**     | runnable or running past its lease and not progressing: a step re-enqueue moves it |
//! | `integration_retry`  | **stuck**     | the retry is exhausted or overdue; a step re-runs the node |
//! | `waiting_for_task`   | waiting       | a person has to act; the platform must not push it       |
//! | `timer`              | waiting       | a date or an event is awaited; nothing to unstick        |
//!
//! A case with any unstickable reason is stuck, even when another of its branches waits for a
//! person. A case with only waiting reasons, or none, is not stuck. A finished case is done.
//! [`CaseLife`] has no waiting state, so `life` answers `Running` for a waiting case; what
//! matters to the remedy is that it is **not** `Stuck`, so `stuck()` omits it and `unstick`
//! refuses it.
//!
//! The reasons come from the insight port's last fold ([`ProcessInsightPort::snapshot`], read with
//! no viewer because the desk filters itself and returns ids only) and the site's own stuck
//! policy (`[services.process_insight].stuck_after_secs`). Without the insight module there is no
//! desk: nothing is provided and the world part says `no cases`, as before.
//!
//! # Unstick
//!
//! `unstick(actor, case, now)`:
//!
//! 1. the actor must hold `process_supervise` (the permission `process_insight` uses for the whole
//!    stuck list), judged on the actor's **own** principal: the actor id is a user record id,
//!    optionally prefixed `agent:` or `user:` (an agent is a user record whose assignments are
//!    its role set). An actor that cannot be resolved, or holds no such role, is refused. The
//!    platform's system principal is never used and never counts;
//! 2. the case must exist on this site and be stuck by [`standing`];
//! 3. one case step is enqueued through [`ProcessHost::unstick_step`], the host's own idempotent
//!    enqueue: a second unstick while the first job is still queued is dropped by the job store;
//! 4. the answer is where the case stands afterwards: `Running` (a step is queued) or `Done`.
//!
//! The call is logged with the actor id and the case id only. No case variable, task content,
//! document value or process name is read, logged or put in an error: every refusal is one of the
//! fixed reasons below.
//!
//! # Ports
//!
//! [`provide`] (called by the process module's finaliser, after every module registered) provides
//! `dyn CaseDesk`. Takes `dyn ProcessInsightPort` (optional) and the site's permission engine and
//! principals.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_bpm_engine::{CaseId, CaseStore};
use spot_bpm_insight::{StuckPolicy, StuckReason};
use spot_core::{Clock, SiteId};
use spot_dag_exec::RunStatus;
use spot_document::engine::PrincipalSource;
use spot_permissions::PermissionEngine;
use spot_remedies::CaseLife;
use time::OffsetDateTime;

use super::ProcessHost;
use crate::permissions::StoragePrincipals;
use crate::services::SiteCtx;
use crate::services::process_insight::{PERM_SUPERVISE, ProcessInsightPort, Settings};
use crate::services::world_access::cases::{CaseDesk, DeskFuture};

/// The most stuck cases one `stuck` call lists.
pub const MAX_STUCK: usize = 1000;

/// Refusal: the actor does not hold the permission to supervise processes (or is not known).
pub const REFUSED_PERMISSION: &str = "the actor may not supervise processes";
/// Refusal: no such case on this site.
pub const REFUSED_NO_CASE: &str = "no such case";
/// Refusal: the case waits for a person or a timer; it is not stuck.
pub const REFUSED_WAITING: &str = "the case is waiting, not stuck";
/// Refusal: the case is running and not stuck.
pub const REFUSED_MOVING: &str = "the case is not stuck";
/// Refusal: the case finished.
pub const REFUSED_DONE: &str = "the case has finished";
/// Refusal: the instant is not a valid time.
pub const REFUSED_TIME: &str = "the instant is not valid";
/// The process engine or its store could not answer.
pub const UNAVAILABLE: &str = "the process engine is not available";
/// The job store did not take the step.
pub const NOT_QUEUED: &str = "the case step could not be queued";

/// Where a case stands, by the desk's rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// The case finished (completed, failed or cancelled).
    Done,
    /// Not progressing, and the platform can clear it.
    Stuck,
    /// Waiting for a person or a timer.
    Waiting,
    /// Progressing, or not yet past what its node expects.
    Moving,
}

impl Standing {
    /// The case's life as the remedy world knows it: `Waiting` and `Moving` are both `Running`.
    #[must_use]
    pub fn life(self) -> CaseLife {
        match self {
            Standing::Done => CaseLife::Done,
            Standing::Stuck => CaseLife::Stuck,
            Standing::Waiting | Standing::Moving => CaseLife::Running,
        }
    }
}

/// Whether the platform can clear a case stuck for `reason` by re-enqueueing its step.
#[must_use]
pub fn is_unstickable(reason: StuckReason) -> bool {
    matches!(reason, StuckReason::Running | StuckReason::IntegrationRetry)
}

/// THE rule: where a case stands, from whether it finished and the insight reasons of its stuck
/// attempts. A finished case is `Done`; any unstickable reason makes it `Stuck`; otherwise any
/// reason at all (a task or a timer) makes it `Waiting`; no reason is `Moving`.
#[must_use]
pub fn standing(finished: bool, reasons: &[StuckReason]) -> Standing {
    if finished {
        Standing::Done
    } else if reasons.iter().copied().any(is_unstickable) {
        Standing::Stuck
    } else if reasons.is_empty() {
        Standing::Moving
    } else {
        Standing::Waiting
    }
}

/// A boxed `Send` future.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the stuck attempts come from: `(case id, reason)` for every open attempt past what its
/// node expects, at `now`. Ids and reason codes only.
pub trait StuckSource: Send + Sync {
    /// The stuck attempts at `now`.
    fn stuck_at(&self, now: OffsetDateTime) -> Vec<(String, StuckReason)>;
}

/// Whether an actor may supervise processes.
pub trait Supervisors: Send + Sync {
    /// `true` only when `actor` resolves to a principal that holds `process_supervise`. An actor
    /// that cannot be resolved is `false`.
    fn may_supervise<'a>(&'a self, actor: &'a str) -> Fut<'a, bool>;
}

/// The production [`StuckSource`]: the insight port's last fold under the site's stuck policy.
pub struct InsightStuck {
    insight: Arc<dyn ProcessInsightPort>,
    site: SiteId,
    policy: StuckPolicy,
}

impl InsightStuck {
    /// The source over `insight` for `site` with the policy of `settings`.
    #[must_use]
    pub fn new(insight: Arc<dyn ProcessInsightPort>, site: SiteId, settings: &Settings) -> Self {
        Self {
            insight,
            site,
            policy: settings.stuck_policy(),
        }
    }
}

impl StuckSource for InsightStuck {
    fn stuck_at(&self, now: OffsetDateTime) -> Vec<(String, StuckReason)> {
        // A snapshot that cannot be read lists nothing: the desk then reports no stuck case,
        // never a guess.
        match self.insight.snapshot(&self.site) {
            Ok(insight) => insight
                .stuck_cases(now, &self.policy)
                .into_iter()
                .map(|case| (case.case.to_string(), case.reason))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// The production [`Supervisors`]: the site's permission engine over the principal of the actor.
pub struct EngineSupervisors {
    engine: Arc<PermissionEngine>,
    principals: Arc<StoragePrincipals>,
}

impl EngineSupervisors {
    /// Supervisors judged by `engine`, with principals read from `principals`.
    #[must_use]
    pub fn new(engine: Arc<PermissionEngine>, principals: Arc<StoragePrincipals>) -> Self {
        Self { engine, principals }
    }
}

/// The user record an actor names: `agent:<id>`, `user:<id>` or the bare id.
fn actor_user(actor: &str) -> Option<spot_app_api::prelude::RecordId> {
    let id = actor
        .strip_prefix("agent:")
        .or_else(|| actor.strip_prefix("user:"))
        .unwrap_or(actor);
    spot_bpm_documents::ids::parse_record_id(id)
}

impl Supervisors for EngineSupervisors {
    fn may_supervise<'a>(&'a self, actor: &'a str) -> Fut<'a, bool> {
        Box::pin(async move {
            let Some(user) = actor_user(actor) else {
                return false;
            };
            let principal = self.principals.resolve(&user).await;
            !principal.is_system
                && self
                    .engine
                    .app_permission(&principal, PERM_SUPERVISE)
                    .allowed
        })
    }
}

/// The cases desk over the process host.
pub struct ProcessCaseDesk {
    host: Arc<ProcessHost>,
    stuck: Arc<dyn StuckSource>,
    supervisors: Arc<dyn Supervisors>,
    clock: Arc<dyn Clock>,
}

impl ProcessCaseDesk {
    /// A desk over `host`; `clock` is the injected clock `stuck` and `life` judge at.
    #[must_use]
    pub fn new(
        host: Arc<ProcessHost>,
        stuck: Arc<dyn StuckSource>,
        supervisors: Arc<dyn Supervisors>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            host,
            stuck,
            supervisors,
            clock,
        }
    }

    /// The reasons of the stuck attempts of each case, at `now`.
    fn reasons_by_case(&self, now: OffsetDateTime) -> BTreeMap<String, Vec<StuckReason>> {
        let mut by_case: BTreeMap<String, Vec<StuckReason>> = BTreeMap::new();
        for (case, reason) in self.stuck.stuck_at(now) {
            by_case.entry(case).or_default().push(reason);
        }
        by_case
    }

    /// Where `case` stands at `now`, or `None` when no such case exists on this site.
    async fn standing_of(
        &self,
        case: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Standing>, &'static str> {
        let Ok(id) = CaseId::new(case) else {
            return Ok(None);
        };
        let engine = self.host.engine().map_err(|_| UNAVAILABLE)?;
        let record = engine.store().load(&id).await.map_err(|_| UNAVAILABLE)?;
        let Some(record) = record else {
            return Ok(None);
        };
        let finished = record.run.status != RunStatus::Running || record.run.is_finished();
        let reasons = self.reasons_by_case(now).remove(case).unwrap_or_default();
        Ok(Some(standing(finished, &reasons)))
    }
}

impl CaseDesk for ProcessCaseDesk {
    fn stuck(&self) -> DeskFuture<'_, Vec<String>> {
        Box::pin(async move {
            let now = self.clock.now_utc();
            let ids: BTreeSet<String> = self
                .reasons_by_case(now)
                .into_iter()
                .filter(|(_, reasons)| standing(false, reasons) == Standing::Stuck)
                .map(|(case, _)| case)
                .collect();
            Ok(ids.into_iter().take(MAX_STUCK).collect())
        })
    }

    fn life<'a>(&'a self, case: &'a str) -> DeskFuture<'a, Option<CaseLife>> {
        Box::pin(async move {
            let now = self.clock.now_utc();
            Ok(self.standing_of(case, now).await?.map(Standing::life))
        })
    }

    fn unstick<'a>(&'a self, actor: &'a str, case: &'a str, now: i64) -> DeskFuture<'a, CaseLife> {
        Box::pin(async move {
            if !self.supervisors.may_supervise(actor).await {
                return Err(REFUSED_PERMISSION);
            }
            let at = OffsetDateTime::from_unix_timestamp(now).map_err(|_| REFUSED_TIME)?;
            match self.standing_of(case, at).await? {
                None => return Err(REFUSED_NO_CASE),
                Some(Standing::Stuck) => {}
                Some(Standing::Waiting) => return Err(REFUSED_WAITING),
                Some(Standing::Moving) => return Err(REFUSED_MOVING),
                Some(Standing::Done) => return Err(REFUSED_DONE),
            }
            let id = CaseId::new(case).map_err(|_| REFUSED_NO_CASE)?;
            self.host
                .unstick_step(&id, at)
                .await
                .map_err(|_| NOT_QUEUED)?;
            // The ids only: no variable, task or document value is read or logged here.
            tracing::info!(actor = %actor, case = %case, "case.unstick: a step was queued");
            // A step is queued, so the case is moving again; the insight fold catches up on its
            // next turn. A case that finished meanwhile answers `Done`.
            let after = self.standing_of(case, at).await?;
            Ok(match after {
                Some(Standing::Done) => CaseLife::Done,
                _ => CaseLife::Running,
            })
        })
    }
}

/// Provides `dyn CaseDesk` over `host`, when the site has the insight module (the definition of
/// stuck comes from it) and the permission assembly. Nothing otherwise. Called from the process
/// module's finaliser, when every module's port is in place.
///
/// # Errors
/// A setting that does not decode, or a port another module already provided.
pub fn provide(ctx: &mut SiteCtx<'_>, host: &Arc<ProcessHost>) -> crate::Result<()> {
    let Some(insight) = ctx.port::<dyn ProcessInsightPort>() else {
        tracing::info!("process: no insight module, so there is no case desk (no stuck rule)");
        return Ok(());
    };
    let settings: Settings = ctx.settings_or_default("process_insight")?;
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(crate::error)?;
    let supervisors: Arc<dyn Supervisors> = Arc::new(EngineSupervisors::new(
        Arc::clone(&env.permission.decision),
        Arc::clone(&env.permission.principals),
    ));
    let stuck: Arc<dyn StuckSource> = Arc::new(InsightStuck::new(insight, site, &settings));
    let desk: Arc<dyn CaseDesk> = Arc::new(ProcessCaseDesk::new(
        Arc::clone(host),
        stuck,
        supervisors,
        Arc::clone(ctx.clock()),
    ));
    ctx.provide(desk)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ALL: [StuckReason; 4] = [
        StuckReason::WaitingForTask,
        StuckReason::Timer,
        StuckReason::IntegrationRetry,
        StuckReason::Running,
    ];

    #[test]
    fn only_running_and_integration_retry_can_be_unstuck() {
        let unstickable: Vec<StuckReason> =
            ALL.iter().copied().filter(|r| is_unstickable(*r)).collect();
        assert_eq!(
            unstickable,
            vec![StuckReason::IntegrationRetry, StuckReason::Running]
        );
    }

    #[test]
    fn the_rule_finished_wins_then_unstickable_then_waiting_then_moving() {
        assert_eq!(standing(true, &[StuckReason::Running]), Standing::Done);
        assert_eq!(standing(true, &[]), Standing::Done);
        assert_eq!(standing(false, &[StuckReason::Running]), Standing::Stuck);
        assert_eq!(
            standing(false, &[StuckReason::IntegrationRetry]),
            Standing::Stuck
        );
        assert_eq!(
            standing(false, &[StuckReason::WaitingForTask]),
            Standing::Waiting
        );
        assert_eq!(standing(false, &[StuckReason::Timer]), Standing::Waiting);
        assert_eq!(standing(false, &[]), Standing::Moving);
    }

    #[test]
    fn a_waiting_branch_does_not_hide_a_stuck_one() {
        assert_eq!(
            standing(false, &[StuckReason::WaitingForTask, StuckReason::Running]),
            Standing::Stuck
        );
    }

    #[test]
    fn only_stuck_is_stuck_in_the_worlds_terms() {
        assert_eq!(Standing::Stuck.life(), CaseLife::Stuck);
        assert_eq!(Standing::Waiting.life(), CaseLife::Running);
        assert_eq!(Standing::Moving.life(), CaseLife::Running);
        assert_eq!(Standing::Done.life(), CaseLife::Done);
    }

    #[test]
    fn an_actor_names_a_user_record_with_or_without_a_prefix() {
        let id = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d6e";
        let bare = actor_user(id).unwrap();
        assert_eq!(actor_user(&format!("agent:{id}")), Some(bare));
        assert_eq!(actor_user(&format!("user:{id}")), Some(bare));
        assert!(actor_user("agent:not-an-id").is_none());
        assert!(actor_user("").is_none());
    }
}
