//! Scheduled scripts: a TypeScript function that runs on a cron schedule as its `run_as` user.
//!
//! A scheduled run has no triggering user, so the user the author named is the only identity it
//! can have. If that user is missing or disabled the run is refused with
//! `serverscript.run_as_unavailable`; it never falls back to a system principal. (Elevation is a
//! separate, audited, per-script flag that does not exist for this kind.)

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::prelude::{CommandId, RecordId, SiteId};
use spot_app_api::{Actor, AppError, AppResult, BoxFuture, ErrorClass, Invocation, StepName};
use spot_manifest::Queue;
use spot_scheduler::slot::rfc3339;
use spot_scheduler::{MissedRunPolicy, RunSlot, Schedule, TimeZone};
use spot_ts::Compiled;

use super::ScriptKind;
use super::api::KindDeps;
use super::exec::{Execution, execute, prepare};
use super::record::{MissedRun, ScheduledScript};
use super::store::KindRun;
use crate::codes;

/// The job handler name a scheduled run is enqueued under; its arguments are
/// `{ "script": "<script name>" }`.
pub const RUN_SCHEDULED_SCRIPT_JOB: &str = "scripting:run_scheduled_script";

/// How much longer than its own timeout a scheduled run's job may take before the scheduler's
/// budget gives up on it (the engine's deadline fires first).
const BUDGET_SLACK: Duration = Duration::from_secs(5);

/// A user, as the run-as lookup sees one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRecord {
    /// The user's record id: the identity the run acts as.
    pub id: RecordId,
    /// Whether the user may act at all.
    pub enabled: bool,
}

/// Finds users by name. A host implements it over the `user` DocType.
pub trait UserDirectory: Send + Sync {
    /// The user named `name`, or `None` when there is none.
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<Option<UserRecord>>>;
}

/// What the scheduler enqueues when a script's schedule fires.
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduleTarget {
    /// The job handler name ([`RUN_SCHEDULED_SCRIPT_JOB`]).
    pub job: String,
    /// The job's arguments: `{ "script": <name> }`.
    pub args: Json,
    /// The queue the job goes on.
    pub queue: Queue,
}

/// The failure state the runner keeps per script (the store keeps the durable one).
#[derive(Default)]
struct ScriptState {
    consecutive_failures: u32,
    disabled: Option<String>,
}

/// Runs scheduled scripts and describes their schedules.
pub struct ScheduledScriptRunner {
    site: SiteId,
    scripts: BTreeMap<String, ScheduledScript>,
    deps: KindDeps,
    users: Arc<dyn UserDirectory>,
    compiled: Mutex<HashMap<String, Arc<Compiled>>>,
    state: Mutex<HashMap<String, ScriptState>>,
}

impl ScheduledScriptRunner {
    /// A runner for the enabled `scripts` of `site`.
    pub fn new(
        site: SiteId,
        scripts: Vec<ScheduledScript>,
        deps: KindDeps,
        users: Arc<dyn UserDirectory>,
    ) -> ScheduledScriptRunner {
        let scripts = scripts
            .into_iter()
            .filter(|script| script.enabled)
            .map(|script| (script.name.clone(), script))
            .collect();
        ScheduledScriptRunner {
            site,
            scripts,
            deps,
            users,
            compiled: Mutex::new(HashMap::new()),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// The schedule of one script, with its missed-run policy and what to enqueue.
    ///
    /// # Errors
    ///
    /// `serverscript.schedule_invalid` when the cron or the zone cannot be scheduled.
    pub fn schedule_of(
        &self,
        script: &ScheduledScript,
    ) -> AppResult<(Schedule, MissedRunPolicy, ScheduleTarget)> {
        let invalid = |why: String| {
            AppError::new(
                ErrorClass::Validation,
                codes::SCHEDULE_INVALID,
                format!("The schedule of `{}` cannot be used: {why}", script.name),
            )
            .detail("script", script.name.as_str())
        };
        let step = StepName::new(RUN_SCHEDULED_SCRIPT_JOB).map_err(|e| invalid(e.to_string()))?;
        let zone = TimeZone::parse(&script.time_zone).map_err(|e| invalid(e.to_string()))?;
        let budget = self.deps.limits.timeout_for_ms(script.timeout_ms) + BUDGET_SLACK;
        let schedule = Schedule::build(
            script.name.clone(),
            script.cron.clone(),
            step,
            script.queue,
            budget,
            zone,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let policy = match script.missed_run {
            MissedRun::Skip => MissedRunPolicy::Skip,
            MissedRun::RunOnce => MissedRunPolicy::RunOnce,
        };
        let target = ScheduleTarget {
            job: RUN_SCHEDULED_SCRIPT_JOB.to_owned(),
            args: json!({ "script": script.name }),
            queue: script.queue,
        };
        Ok((schedule, policy, target))
    }

    /// The schedules of every enabled script that is not disabled in memory, in name order, for
    /// the host's scheduler. A script whose schedule cannot be built (see
    /// [`ScheduledScriptRunner::schedule_of`]) is left out.
    pub fn schedules(&self) -> Vec<(Schedule, MissedRunPolicy, ScheduleTarget)> {
        self.scripts
            .values()
            .filter(|script| self.disabled_reason(&script.name).is_none())
            .filter_map(|script| self.schedule_of(script).ok())
            .collect()
    }

    /// The scripts disabled in memory, as `(name, reason)`.
    pub fn disabled_scripts(&self) -> Vec<(String, String)> {
        self.state()
            .iter()
            .filter_map(|(name, state)| {
                state
                    .disabled
                    .as_ref()
                    .map(|reason| (name.clone(), reason.clone()))
            })
            .collect()
    }

    /// Runs the script `script` for `slot`, as its `run_as` user.
    ///
    /// The slot's deduplication key is the run's command id and is passed to the script as
    /// `ctx.deduplication_key`. Every outcome after the script is found is recorded in the store
    /// as a [`KindRun`], and a failure counts toward disabling the script.
    ///
    /// # Errors
    ///
    /// - `serverscript.run_as_unavailable` when the `run_as` user is missing or disabled (the
    ///   engine is never entered);
    /// - the script's own error otherwise (a limit, a typed throw, the host's permission
    ///   error, ...);
    /// - `serverscript.record_invalid` when no enabled script has that name, or when it was
    ///   disabled after repeated failures.
    pub async fn run(&self, script: &str, slot: &RunSlot) -> AppResult<KindRun> {
        let Some(record) = self.scripts.get(script) else {
            return Err(AppError::new(
                ErrorClass::NotFound,
                codes::RECORD_INVALID,
                format!("There is no enabled scheduled script `{script}`."),
            ));
        };
        if let Some(reason) = self.disabled_reason(script) {
            return Err(AppError::new(
                ErrorClass::InvalidState,
                codes::RECORD_INVALID,
                format!("The scheduled script `{script}` is disabled: {reason}"),
            ));
        }

        // Who: the named user, or nobody. Never the platform.
        let user = match self.users.find(&record.run_as).await {
            Ok(Some(user)) if user.enabled => user,
            Ok(_) => {
                let error = AppError::new(
                    ErrorClass::InvalidState,
                    codes::RUN_AS_UNAVAILABLE,
                    format!(
                        "The user `{}` the script `{script}` runs as is missing or disabled.",
                        record.run_as
                    ),
                )
                .detail("script", script);
                self.record_failure(record, &error.code, "", 0, 0).await;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let user_text = user.id.to_string();

        let compiled = match self.compiled(record) {
            Ok(compiled) => compiled,
            Err(error) => {
                self.record_failure(record, &error.code, &user_text, 0, 0)
                    .await;
                return Err(error);
            }
        };
        let invocation = Invocation {
            site: self.site.clone(),
            actor: Actor::User {
                id: user.id,
                via: None,
            },
            command_id: CommandId::new(&slot.deduplication_key).ok(),
            language: None,
            now: self.deps.clock.now_utc(),
        };
        let entry_args = json!({
            "ctx": {
                "schedule": record.name,
                "scheduled_for": rfc3339(slot.scheduled_for),
                "deduplication_key": slot.deduplication_key,
                "user": user_text,
            },
        });
        let mut access = self.deps.access.open(invocation);
        let executed = execute(
            Execution {
                engine: &self.deps.engine,
                compiled: &compiled,
                entry: ScriptKind::Scheduled.entry(),
                args: &entry_args,
                limits: self.deps.limits.engine_limits_for_ms(record.timeout_ms),
                name: &record.name,
                currencies: self.deps.currencies.clone(),
            },
            access.as_mut(),
        )
        .await;

        match executed.result {
            Ok(_) => {
                self.state()
                    .entry(record.name.clone())
                    .or_default()
                    .consecutive_failures = 0;
                let run = KindRun {
                    script: record.name.clone(),
                    kind: ScriptKind::Scheduled,
                    code: "ok".to_owned(),
                    ok: true,
                    duration_ms: executed.duration_ms,
                    host_calls: executed.host_calls,
                    at: self.deps.clock.now_utc(),
                    user: user_text,
                    message: executed.message,
                };
                let _ = self.deps.store.record_run(run.clone()).await;
                Ok(run)
            }
            Err(error) => {
                self.record_failure(
                    record,
                    &error.code,
                    &user_text,
                    executed.duration_ms,
                    executed.host_calls,
                )
                .await;
                Err(error)
            }
        }
    }

    /// The compiled script, prepared once per runner.
    fn compiled(&self, script: &ScheduledScript) -> AppResult<Arc<Compiled>> {
        if let Some(found) = self
            .compiled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&script.name)
        {
            return Ok(Arc::clone(found));
        }
        let compiled = prepare(
            &self.deps.engine,
            ScriptKind::Scheduled,
            &script.name,
            &script.source,
        )?;
        self.compiled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(script.name.clone(), Arc::clone(&compiled));
        Ok(compiled)
    }

    /// Records a failed run, counts it, and disables the script after a run of failures.
    async fn record_failure(
        &self,
        script: &ScheduledScript,
        code: &str,
        user: &str,
        duration_ms: u64,
        host_calls: u32,
    ) {
        let failures = {
            let mut state = self.state();
            let entry = state.entry(script.name.clone()).or_default();
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.consecutive_failures
        };
        let run = KindRun {
            script: script.name.clone(),
            kind: ScriptKind::Scheduled,
            code: code.to_owned(),
            ok: false,
            duration_ms,
            host_calls,
            at: self.deps.clock.now_utc(),
            user: user.to_owned(),
            message: None,
        };
        let _ = self.deps.store.record_run(run).await;
        if failures >= self.deps.limits.failure_threshold {
            let reason = format!("{failures} consecutive failures; last: {code}");
            {
                let mut state = self.state();
                let entry = state.entry(script.name.clone()).or_default();
                if entry.disabled.is_some() {
                    return;
                }
                entry.disabled = Some(reason.clone());
            }
            let _ = self
                .deps
                .store
                .disable(ScriptKind::Scheduled, &script.name, &reason)
                .await;
        }
    }

    fn disabled_reason(&self, name: &str) -> Option<String> {
        self.state()
            .get(name)
            .and_then(|state| state.disabled.clone())
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, ScriptState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
