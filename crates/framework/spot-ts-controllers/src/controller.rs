//! A TypeScript controller: the [`Controller`] an installed module becomes.
//!
//! Its steps and methods are the ones the module declared. Each execution calls the module's
//! `run_step`/`run_method` entry on a fresh runtime, as the triggering user: the document view and
//! every host call go through the step's [`HookContext`] or the method's [`MethodContext`], so the
//! user's rights decide what the handler sees and does. A handler's own typed error becomes the
//! same `AppError` a native step raises; a limit breach fails the operation; an `after_commit` step
//! that keeps failing is disabled with a visible reason, as in Server Scripts.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, ErrorClass, HookContext, Invocation,
    MethodContext, MethodDecl, Phase, Repository, StepDecl, StepName,
};
use spot_money::currency::CurrencyTable;
use spot_server_scripts::{
    AllowAllView, FieldView, HostRequest, MethodCaller, ScriptBridge, ScriptLog, ScriptRun,
    TypedThrow, document_json, patch_from_json, typed_throw_error,
};
use spot_ts::{Compiled, HostBridge, ScriptEngine, ScriptError};

use crate::bridge::service_repo;
use crate::codes;
use crate::contract::{Contract, MethodSpec, StepSpec};
use crate::error::map_run_error;
use crate::limits::TsLimits;
use crate::module::LoadedModule;
use crate::schema::{validate_args, validate_result};

/// What one execution produced beyond its result.
struct RunStats {
    host_calls: u32,
    typed_throw: Option<TypedThrow>,
    message: Option<String>,
}

/// The failure state of one step, kept in memory (an `after_commit` step is disabled in this set).
#[derive(Default)]
struct StepState {
    consecutive_failures: u32,
    disabled: bool,
    reason: Option<String>,
}

/// An installed TypeScript controller module, as the document engine sees it.
pub struct TsController {
    app: String,
    module: String,
    doctype: String,
    steps: Vec<StepSpec>,
    methods: BTreeMap<String, MethodSpec>,
    by_step: BTreeMap<String, usize>,
    engine: ScriptEngine,
    compiled: Arc<Compiled>,
    view: Arc<dyn FieldView>,
    limits: TsLimits,
    log: Arc<ScriptLog>,
    currencies: Option<Arc<CurrencyTable>>,
    method_caller: Option<Arc<dyn MethodCaller>>,
    state: Mutex<HashMap<String, StepState>>,
}

impl TsController {
    /// Builds a controller from a loaded module registered under `app`.
    pub fn new(
        app: &str,
        module: &str,
        loaded: LoadedModule,
        engine: ScriptEngine,
    ) -> TsController {
        TsController::from_parts(app, module, loaded, engine, TsLimits::default())
    }

    /// Builds a controller with these limits.
    pub fn from_parts(
        app: &str,
        module: &str,
        loaded: LoadedModule,
        engine: ScriptEngine,
        limits: TsLimits,
    ) -> TsController {
        let Contract {
            doctype,
            steps,
            methods,
        } = loaded.contract;
        let by_step = steps
            .iter()
            .enumerate()
            .map(|(index, step)| (step.name.clone(), index))
            .collect();
        TsController {
            app: app.to_owned(),
            module: module.to_owned(),
            doctype,
            steps,
            methods,
            by_step,
            engine,
            compiled: loaded.compiled,
            view: Arc::new(AllowAllView),
            log: Arc::new(ScriptLog::new(limits.log_capacity)),
            limits,
            currencies: None,
            method_caller: None,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// The same controller with the host's document view (field access).
    #[must_use]
    pub fn with_view(mut self, view: Arc<dyn FieldView>) -> TsController {
        self.view = view;
        self
    }

    /// The same controller recording runs into `log`.
    #[must_use]
    pub fn with_log(mut self, log: Arc<ScriptLog>) -> TsController {
        self.log = log;
        self
    }

    /// The same controller with a currency table for `host.format_money`.
    #[must_use]
    pub fn with_currencies(mut self, currencies: Arc<CurrencyTable>) -> TsController {
        self.currencies = Some(currencies);
        self
    }

    /// The same controller able to dispatch `host.call_method` through `caller`.
    #[must_use]
    pub fn with_method_caller(mut self, caller: Arc<dyn MethodCaller>) -> TsController {
        self.method_caller = Some(caller);
        self
    }

    /// The app the module is installed as.
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The module's path within the app (`controllers/invoice.ts`).
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The run log.
    pub fn log(&self) -> &Arc<ScriptLog> {
        &self.log
    }

    /// The steps disabled in memory, as `(step, reason)`.
    pub fn disabled_steps(&self) -> Vec<(String, String)> {
        self.state()
            .iter()
            .filter(|(_, state)| state.disabled)
            .map(|(name, state)| {
                (
                    name.clone(),
                    state
                        .reason
                        .clone()
                        .unwrap_or_else(|| "disabled".to_owned()),
                )
            })
            .collect()
    }

    /// The JSON Schemas of the methods, as the API layer serves them.
    pub fn schemas(&self) -> Json {
        let contract = Contract {
            doctype: self.doctype.clone(),
            steps: self.steps.clone(),
            methods: self.methods.clone(),
        };
        contract.schemas()
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, StepState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn is_disabled(&self, step: &str) -> bool {
        self.state().get(step).is_some_and(|state| state.disabled)
    }

    /// Runs one entry of the module, servicing its host calls against `repo` on this task.
    async fn execute(
        &self,
        entry: &str,
        payload: &Json,
        repo: &mut dyn Repository,
        inv: &Invocation,
        timeout_ms: u64,
    ) -> (Result<Json, ScriptError>, RunStats) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<HostRequest>();
        let now = now_millis(inv);
        let bridge = Arc::new(ScriptBridge::new(
            sender,
            now,
            seed_for(&self.module, now),
            self.currencies.clone(),
        ));
        let run = self.engine.run(
            &self.compiled,
            entry,
            payload,
            Arc::clone(&bridge) as Arc<dyn HostBridge>,
            self.limits.engine_limits(timeout_ms),
        );
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                request = receiver.recv() => {
                    if let Some(request) = request {
                        let reply = service_repo(
                            repo,
                            inv,
                            self.method_caller.as_deref(),
                            &request.name,
                            request.args,
                        )
                        .await;
                        let _ = request.reply.send(reply);
                    }
                }
            }
        };
        let stats = RunStats {
            host_calls: bridge.host_calls(),
            typed_throw: bridge.typed_throw(),
            message: bridge.last_message(),
        };
        (result, stats)
    }

    fn record_failure(&self, step: &str, code: &str, now: OffsetDateTime, duration_ms: u64) {
        let failures = {
            let mut state = self.state();
            let entry = state.entry(step.to_owned()).or_default();
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.consecutive_failures
        };
        self.log.record(ScriptRun {
            script: step.to_owned(),
            doctype: self.doctype.clone(),
            event: spot_server_scripts::ScriptEvent::AfterCommit,
            code: code.to_owned(),
            ok: false,
            duration_ms,
            host_calls: 0,
            at: now,
            message: None,
        });
        if failures >= self.limits.failure_threshold {
            let mut state = self.state();
            let entry = state.entry(step.to_owned()).or_default();
            if !entry.disabled {
                entry.disabled = true;
                entry.reason = Some(format!("{failures} consecutive failures; last: {code}"));
            }
        }
    }

    fn record_success(&self, step: &str, stats: &RunStats, now: OffsetDateTime, duration_ms: u64) {
        {
            let mut state = self.state();
            state
                .entry(step.to_owned())
                .or_default()
                .consecutive_failures = 0;
        }
        self.log.record(ScriptRun {
            script: step.to_owned(),
            doctype: self.doctype.clone(),
            event: spot_server_scripts::ScriptEvent::AfterCommit,
            code: "ok".to_owned(),
            ok: true,
            duration_ms,
            host_calls: stats.host_calls,
            at: now,
            message: stats.message.clone(),
        });
    }

    /// The payload a step handler sees.
    fn step_payload(
        &self,
        step: &StepSpec,
        inv: &Invocation,
        doc: &Document,
        view: &Document,
    ) -> Json {
        json!({
            "step": step.name,
            "doc": document_json(view, None),
            "ctx": {
                "doctype": self.doctype,
                "id": doc.id().to_string(),
                "event": step.point.name(),
                "language": inv.language,
                "now": format_datetime(inv.now),
            },
        })
    }

    /// The payload a method handler sees.
    fn method_payload(
        &self,
        method: &MethodSpec,
        inv: &Invocation,
        doc: &Document,
        args: &Json,
    ) -> Json {
        json!({
            "method": method.name,
            "doc": document_json(doc, None),
            "args": args,
            "ctx": {
                "doctype": self.doctype,
                "id": doc.id().to_string(),
                "method": method.name,
                "language": inv.language,
                "now": format_datetime(inv.now),
            },
        })
    }
}

impl Controller for TsController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps
            .iter()
            .filter_map(|step| {
                StepName::new(&format!("{}:{}", self.app, step.name))
                    .ok()
                    .map(|name| StepDecl {
                        name,
                        point: step.point.clone(),
                        priority: step.priority,
                        budget: self.limits.timeout(step.timeout_ms) + Duration::from_millis(50),
                    })
            })
            .collect()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        Contract {
            doctype: self.doctype.clone(),
            steps: self.steps.clone(),
            methods: self.methods.clone(),
        }
        .method_decls()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let Some(&index) = self.by_step.get(
                step.as_str()
                    .split_once(':')
                    .map_or(step.as_str(), |(_, name)| name),
            ) else {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "controller.no_step",
                    format!("The TypeScript controller has no step `{step}`."),
                ));
            };
            let spec = &self.steps[index];
            if self.is_disabled(&spec.name) {
                return Ok(());
            }
            let inv = ctx.invocation().clone();
            let now = inv.now;
            let started = Instant::now();
            let committed = spec.point.phase() == Phase::Committed;

            let def = ctx.meta(&self.doctype).await?;
            let view = self.view.readable(&inv, &def, ctx.doc()).await?;
            let payload = self.step_payload(spec, &inv, ctx.doc(), &view);
            let (result, stats) = self
                .execute("run_step", &payload, ctx, &inv, spec.timeout_ms)
                .await;
            let duration_ms = elapsed(started);

            if let Some(thrown) = &stats.typed_throw {
                let error = typed_throw_error(thrown, &self.module);
                self.record_failure(&spec.name, &error.code, now, duration_ms);
                return if committed { Ok(()) } else { Err(error) };
            }
            match result {
                Ok(value) => {
                    if let Some(patch) = extract_patch(&value) {
                        // The phase rules are the engine's: `doc_mut` refuses a change outside
                        // `before_*` with `platform.phase_violation`, exactly as it does for a
                        // native controller, so both behave alike.
                        let patch = patch_from_json(&def, &patch, RecordId::new_v7)?;
                        ctx.doc_mut()?.apply_patch(&patch, RecordId::new_v7)?;
                    }
                    self.record_success(&spec.name, &stats, now, duration_ms);
                    Ok(())
                }
                Err(error) => {
                    let mapped = map_run_error(error, &self.module);
                    self.record_failure(&spec.name, &mapped.code, now, duration_ms);
                    if committed { Ok(()) } else { Err(mapped) }
                }
            }
        })
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let Some(spec) = self.methods.get(method) else {
                return Err(AppError::new(
                    ErrorClass::MethodNotFound,
                    "method.not_found",
                    format!("no whitelisted method `{method}` on this controller"),
                ));
            };
            validate_args(&spec.args, &args)?;
            let inv = ctx.invocation().clone();
            let payload = self.method_payload(spec, &inv, ctx.doc(), &args);
            let (result, stats) = self.execute("run_method", &payload, ctx, &inv, 0).await;
            if let Some(thrown) = &stats.typed_throw {
                return Err(typed_throw_error(thrown, &self.module));
            }
            match result {
                Ok(value) => {
                    if let Err(why) = validate_result(&spec.result, &value) {
                        return Err(AppError::new(
                            ErrorClass::Internal,
                            codes::RESULT_INVALID,
                            format!("The method `{method}` returned a result that does not match its declared schema."),
                        )
                        .detail("module", self.module.as_str())
                        .detail("cause", why));
                    }
                    Ok(value)
                }
                Err(error) => Err(map_run_error(error, &self.module)),
            }
        })
    }
}

/// The milliseconds since `started`, saturating.
fn elapsed(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The patch a handler returned: an explicit `{ patch: {..} }`, non-empty. Anything else is not a
/// patch.
pub(crate) fn extract_patch(value: &Json) -> Option<Json> {
    let patch = value.as_object()?.get("patch")?;
    match patch.as_object() {
        Some(object) if object.is_empty() => None,
        Some(_) => Some(patch.clone()),
        None => None,
    }
}

/// The invocation's time in milliseconds since the Unix epoch (the only clock a handler sees).
fn now_millis(inv: &Invocation) -> i64 {
    inv.now
        .unix_timestamp()
        .saturating_mul(1_000)
        .saturating_add(i64::from(inv.now.nanosecond() / 1_000_000))
}

/// A deterministic seed for a module's pseudo-random numbers: the same module at the same
/// invocation time gives the same sequence.
fn seed_for(module: &str, now_millis: i64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in module.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for byte in now_millis.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn format_datetime(dt: OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| dt.unix_timestamp().to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_patch_is_explicit() {
        assert_eq!(
            extract_patch(&json!({"patch": {"total": "1"}})),
            Some(json!({"total": "1"}))
        );
        assert_eq!(extract_patch(&json!({"patch": {}})), None);
        assert_eq!(extract_patch(&json!({"total": "1"})), None);
    }

    #[test]
    fn seeds_are_deterministic() {
        assert_eq!(seed_for("a", 5), seed_for("a", 5));
        assert_ne!(seed_for("a", 5), seed_for("a", 6));
    }
}
