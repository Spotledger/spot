//! The script controller: one step per (script, event), run on the embedded engine.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, ErrorClass, HookContext, Phase, StepDecl, StepName,
};
use spot_money::currency::CurrencyTable;
use spot_ts::{Compiled, HostBridge, ScriptEngine, ScriptError, ScriptId};
use spot_ts_transpile::{Diagnostic, Options};

use crate::SERVER_SCRIPT_DOCTYPE;
use crate::codes;
use crate::host::{
    HostRequest, MethodCaller, ScriptBridge, TypedThrow, service, typed_throw_error,
};
use crate::limits::ScriptLimits;
use crate::log::{ScriptLog, ScriptRun};
use crate::record::{FIELD_SCRIPT, ServerScript};
use crate::store::ScriptStore;
use crate::view::{AllowAllView, FieldView, document_json, patch_from_json};

/// A script's compiled JavaScript, kept until its source changes.
struct Prepared {
    js: String,
    source_map: String,
}

/// What one execution produced beyond its result.
struct RunStats {
    host_calls: u32,
    typed_throw: Option<TypedThrow>,
    message: Option<String>,
}

/// One script and the step it runs as.
struct Entry {
    script: ServerScript,
    step: StepName,
}

/// The per-script failure state the controller keeps in memory (the store keeps the durable one).
#[derive(Default)]
struct ScriptState {
    consecutive_failures: u32,
    disabled: bool,
    reason: Option<String>,
}

/// A DocType's document-event scripts, as one controller of steps on the ADR-51 hook points.
pub struct ScriptController {
    doctype: String,
    entries: Vec<Entry>,
    by_step: BTreeMap<String, usize>,
    engine: ScriptEngine,
    store: Arc<dyn ScriptStore>,
    view: Arc<dyn FieldView>,
    limits: ScriptLimits,
    log: Arc<ScriptLog>,
    currencies: Option<Arc<CurrencyTable>>,
    method_caller: Option<Arc<dyn MethodCaller>>,
    prepared: Mutex<HashMap<String, Arc<Prepared>>>,
    state: Mutex<HashMap<String, ScriptState>>,
}

impl ScriptController {
    /// Builds the controller for `doctype` from its enabled `scripts`.
    ///
    /// # Errors
    ///
    /// `controller.no_step` when two scripts need the same step name and cannot be told apart
    /// (only possible for names differing outside storage-name characters).
    pub fn new(
        doctype: &str,
        scripts: Vec<ServerScript>,
        engine: ScriptEngine,
        store: Arc<dyn ScriptStore>,
    ) -> AppResult<ScriptController> {
        let mut taken = BTreeSet::new();
        let mut entries = Vec::with_capacity(scripts.len());
        let mut by_step = BTreeMap::new();
        for script in scripts {
            if !script.enabled {
                continue;
            }
            let step = step_name(&script, &mut taken)?;
            by_step.insert(step.as_str().to_owned(), entries.len());
            entries.push(Entry { script, step });
        }
        Ok(ScriptController {
            doctype: doctype.to_owned(),
            entries,
            by_step,
            engine,
            store,
            view: Arc::new(AllowAllView),
            limits: ScriptLimits::default(),
            log: Arc::new(ScriptLog::new(ScriptLimits::default().log_capacity)),
            currencies: None,
            method_caller: None,
            prepared: Mutex::new(HashMap::new()),
            state: Mutex::new(HashMap::new()),
        })
    }

    /// The same controller with the host's document view (field access).
    #[must_use]
    pub fn with_view(mut self, view: Arc<dyn FieldView>) -> ScriptController {
        self.view = view;
        self
    }

    /// The same controller with these limits.
    #[must_use]
    pub fn with_limits(mut self, limits: ScriptLimits) -> ScriptController {
        self.limits = limits;
        self
    }

    /// The same controller recording runs into `log`.
    #[must_use]
    pub fn with_log(mut self, log: Arc<ScriptLog>) -> ScriptController {
        self.log = log;
        self
    }

    /// The same controller with a currency table for `host.format_money`.
    #[must_use]
    pub fn with_currencies(mut self, currencies: Arc<CurrencyTable>) -> ScriptController {
        self.currencies = Some(currencies);
        self
    }

    /// The same controller able to dispatch `host.call` through `caller`.
    #[must_use]
    pub fn with_method_caller(mut self, caller: Arc<dyn MethodCaller>) -> ScriptController {
        self.method_caller = Some(caller);
        self
    }

    /// The DocType's scripts, in step order.
    pub fn scripts(&self) -> Vec<ServerScript> {
        self.entries.iter().map(|e| e.script.clone()).collect()
    }

    /// The run log.
    pub fn log(&self) -> &Arc<ScriptLog> {
        &self.log
    }

    /// The scripts disabled in memory, as `(name, reason)`.
    pub fn disabled_scripts(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    fn is_disabled(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .is_some_and(|state| state.disabled)
    }

    /// Transpiles a script's source once per distinct source, caching the JavaScript.
    fn prepared(&self, script: &ServerScript) -> AppResult<Arc<Prepared>> {
        let key = format!("{}:{}", script.name, fnv1a(script.source.as_bytes()));
        if let Some(found) = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(Arc::clone(found));
        }
        let options = Options {
            allow_imports: Vec::new(),
            ..Options::default()
        };
        let file = format!("{}.ts", script.name);
        let transpiled = spot_ts_transpile::transpile(&script.source, &file, &options)
            .map_err(|diagnostics| compile_error(script, &diagnostics))?;
        let prepared = Arc::new(Prepared {
            js: transpiled.js,
            source_map: transpiled.source_map,
        });
        self.prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, Arc::clone(&prepared));
        Ok(prepared)
    }

    /// Runs one prepared script with the bridge, servicing its host calls on this task.
    async fn execute(
        &self,
        script: &ServerScript,
        compiled: &Arc<Compiled>,
        args: &Json,
        ctx: &mut dyn HookContext,
    ) -> (Result<Json, ScriptError>, RunStats) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<HostRequest>();
        let now = now_millis(ctx.invocation());
        let bridge = Arc::new(ScriptBridge::new(
            sender,
            now,
            seed_for(&script.name, now),
            self.currencies.clone(),
        ));
        let limits = self.limits.engine_limits(script);
        let run = self.engine.run(
            compiled,
            script.event.entry(),
            args,
            Arc::clone(&bridge) as Arc<dyn HostBridge>,
            limits,
        );
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                request = receiver.recv() => {
                    if let Some(request) = request {
                        let reply = service(
                            ctx,
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

    async fn record_failure(
        &self,
        script: &ServerScript,
        code: &str,
        now: OffsetDateTime,
        duration_ms: u64,
    ) {
        let failures = {
            let mut state = self.state();
            let entry = state.entry(script.name.clone()).or_default();
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.consecutive_failures
        };
        let run = ScriptRun {
            script: script.name.clone(),
            doctype: self.doctype.clone(),
            event: script.event,
            code: code.to_owned(),
            ok: false,
            duration_ms,
            host_calls: 0,
            at: now,
            message: None,
        };
        let _ = self.store.record_run(run.clone()).await;
        self.log.record(run);
        if failures >= self.limits.failure_threshold {
            self.disable(
                script,
                &format!("{failures} consecutive failures; last: {code}"),
            )
            .await;
        }
    }

    fn record_success(
        &self,
        script: &ServerScript,
        stats: &RunStats,
        now: OffsetDateTime,
        duration_ms: u64,
    ) {
        {
            let mut state = self.state();
            let entry = state.entry(script.name.clone()).or_default();
            entry.consecutive_failures = 0;
        }
        self.log.record(ScriptRun {
            script: script.name.clone(),
            doctype: self.doctype.clone(),
            event: script.event,
            code: "ok".to_owned(),
            ok: true,
            duration_ms,
            host_calls: stats.host_calls,
            at: now,
            message: stats.message.clone(),
        });
    }

    async fn disable(&self, script: &ServerScript, reason: &str) {
        {
            let mut state = self.state();
            let entry = state.entry(script.name.clone()).or_default();
            if entry.disabled {
                return;
            }
            entry.disabled = true;
            entry.reason = Some(reason.to_owned());
        }
        let _ = self
            .store
            .disable(&self.doctype, &script.name, reason)
            .await;
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, ScriptState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Controller for ScriptController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.entries
            .iter()
            .map(|entry| StepDecl {
                name: entry.step.clone(),
                point: entry.script.event.hook_point(),
                priority: entry.script.step_priority(),
                // The engine's own deadline fires first; this is the host's backstop.
                budget: self.limits.timeout_for(&entry.script) + Duration::from_millis(50),
            })
            .collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let Some(&index) = self.by_step.get(step.as_str()) else {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "controller.no_step",
                    format!("The script controller has no step `{step}`."),
                ));
            };
            let entry = &self.entries[index];
            let script = &entry.script;
            if self.is_disabled(&script.name) {
                return Ok(());
            }
            let now = ctx.invocation().now;
            let started = Instant::now();

            let def = match ctx.meta(&script.doctype).await {
                Ok(def) => def,
                Err(error) => return Err(error),
            };
            let view = match self.view.readable(ctx.invocation(), &def, ctx.doc()).await {
                Ok(view) => view,
                Err(error) => return Err(error),
            };
            let args = json!({
                "doc": document_json(&view, None),
                "ctx": {
                    "event": script.event.as_str(),
                    "doctype": script.doctype,
                    "id": ctx.doc().id().to_string(),
                },
            });

            let prepared = match self.prepared(script) {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.record_failure(script, &error.code, now, elapsed(started))
                        .await;
                    return Err(error);
                }
            };
            let compiled = match self.engine.compile(
                &prepared.js,
                ScriptId::new(SERVER_SCRIPT_DOCTYPE, &script.name, 0),
                Some(&prepared.source_map),
            ) {
                Ok(compiled) => compiled,
                Err(error) => {
                    let mapped = AppError::field(
                        FieldPath::field(FIELD_SCRIPT),
                        codes::COMPILE_FAILED,
                        format!("The script `{}` did not compile: {error}", script.name),
                    )
                    .detail("script", script.name.as_str());
                    self.record_failure(script, &mapped.code, now, elapsed(started))
                        .await;
                    return Err(mapped);
                }
            };

            let (result, stats) = self.execute(script, &compiled, &args, ctx).await;
            let duration_ms = elapsed(started);

            if let Some(thrown) = &stats.typed_throw {
                let error = typed_throw_error(thrown, &script.name);
                self.record_failure(script, &error.code, now, duration_ms)
                    .await;
                if script.event.phase() == Phase::Committed {
                    return Ok(());
                }
                return Err(error);
            }

            match result {
                Ok(value) => {
                    if let Some(patch) = extract_patch(&value) {
                        if !script.event.may_modify_own() {
                            let error = AppError::new(
                                ErrorClass::Internal,
                                codes::PHASE_VIOLATION,
                                format!(
                                    "The {} script `{}` may not change the document.",
                                    script.event.as_str(),
                                    script.name
                                ),
                            )
                            .detail("script", script.name.as_str());
                            self.record_failure(script, &error.code, now, duration_ms)
                                .await;
                            return Err(error);
                        }
                        let patch = patch_from_json(&def, &patch, RecordId::new_v7)?;
                        let doc = ctx.doc_mut()?;
                        doc.apply_patch(&patch, RecordId::new_v7)?;
                    }
                    self.record_success(script, &stats, now, duration_ms);
                    Ok(())
                }
                Err(error) => {
                    let mapped = crate::error::map_script_error(error, &script.name);
                    self.record_failure(script, &mapped.code, now, duration_ms)
                        .await;
                    if script.event.phase() == Phase::Committed {
                        Ok(())
                    } else {
                        Err(mapped)
                    }
                }
            }
        })
    }
}

/// The milliseconds since `started`, saturating.
fn elapsed(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The step name of one script: `server_script:<name>_<event>`, a valid storage-name pair,
/// unique across the controller's scripts.
fn step_name(script: &ServerScript, taken: &mut BTreeSet<String>) -> AppResult<StepName> {
    let mut base: String = script
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if !base.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        base.insert(0, 's');
    }
    let mut candidate = format!("{base}_{}", script.event.as_str());
    if candidate.len() > 63 {
        candidate.truncate(63);
    }
    let mut suffix = 2;
    while taken.contains(&candidate) {
        let stem: String = candidate.chars().take(58).collect();
        candidate = format!("{stem}_{suffix}");
        suffix += 1;
    }
    taken.insert(candidate.clone());
    StepName::new(&format!("server_script:{candidate}"))
}

/// The patch a script returned: an explicit `{ patch: {..} }`, non-empty. Anything else is not a
/// patch.
fn extract_patch(value: &Json) -> Option<Json> {
    let patch = value.as_object()?.get("patch")?;
    match patch.as_object() {
        Some(object) if object.is_empty() => None,
        Some(_) => Some(patch.clone()),
        None => None,
    }
}

fn compile_error(script: &ServerScript, diagnostics: &[Diagnostic]) -> AppError {
    let mut error: Option<AppError> = None;
    for diagnostic in diagnostics {
        let message = format!(
            "line {}, column {}: {}",
            diagnostic.line, diagnostic.column, diagnostic.message
        );
        error = Some(match error {
            None => AppError::field(FieldPath::field(FIELD_SCRIPT), &diagnostic.code, message),
            Some(existing) => {
                existing.with_field(FieldPath::field(FIELD_SCRIPT), &diagnostic.code, message)
            }
        });
    }
    let error = error.unwrap_or_else(|| {
        AppError::field(
            FieldPath::field(FIELD_SCRIPT),
            codes::COMPILE_FAILED,
            format!("The script `{}` did not compile.", script.name),
        )
    });
    error.detail("script", script.name.as_str())
}

/// The invocation's time in milliseconds since the Unix epoch (the only clock a script sees).
fn now_millis(inv: &spot_app_api::Invocation) -> i64 {
    inv.now
        .unix_timestamp()
        .saturating_mul(1_000)
        .saturating_add(i64::from(inv.now.nanosecond() / 1_000_000))
}

/// A deterministic seed for a script's pseudo-random numbers: the same script at the same
/// invocation time gives the same sequence.
fn seed_for(name: &str, now_millis: i64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for byte in now_millis.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::record::ScriptEvent;

    fn script(name: &str, event: ScriptEvent) -> ServerScript {
        ServerScript {
            record_id: None,
            name: name.into(),
            doctype: "note".into(),
            event,
            source: "export function on_validate() {}".into(),
            compiled_hash: None,
            timeout_ms: 200,
            priority: 0,
            enabled: true,
            disabled_reason: None,
        }
    }

    #[test]
    fn step_names_are_storage_names_and_unique() {
        let mut taken = BTreeSet::new();
        let first = step_name(&script("Check Qty", ScriptEvent::Validate), &mut taken).unwrap();
        let second = step_name(&script("check_qty", ScriptEvent::Validate), &mut taken).unwrap();
        assert_eq!(first.as_str(), "server_script:check_qty_validate");
        assert_ne!(first, second);
        assert_eq!(first.app(), "server_script");
    }

    #[test]
    fn a_patch_is_explicit() {
        assert_eq!(
            extract_patch(&json!({"patch": {"total": "1"}})),
            Some(json!({"total": "1"}))
        );
        assert_eq!(extract_patch(&json!({"patch": {}})), None);
        assert_eq!(extract_patch(&json!({"total": "1"})), None);
        assert_eq!(extract_patch(&Json::Null), None);
    }

    #[test]
    fn seeds_are_deterministic() {
        assert_eq!(seed_for("a", 5), seed_for("a", 5));
        assert_ne!(seed_for("a", 5), seed_for("a", 6));
    }
}
