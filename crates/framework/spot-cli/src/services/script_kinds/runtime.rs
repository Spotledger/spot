//! The runtime: documents to registries, kept current, and the one place a script runs from.

use super::*;
use spot_server_scripts::elevation::ElevationWiring;
use spot_server_scripts::kinds::{API_SCRIPT_DOCTYPE, SCHEDULED_SCRIPT_DOCTYPE};

/// What the module is doing right now.
#[derive(Clone)]
enum State {
    /// Assembly has not finished.
    Pending,
    /// Off: no `scripting` app, `enabled = false`.
    Off,
    /// Running.
    Ready(Arc<KindsRuntime>),
}

/// The per-site handle the admin controllers (installed before the engine exists) and `register`
/// share.
pub struct KindsHandle {
    state: RwLock<State>,
}

impl KindsHandle {
    fn new() -> KindsHandle {
        KindsHandle {
            state: RwLock::new(State::Pending),
        }
    }

    /// A handle not shared with any site, for tests.
    #[must_use]
    pub fn detached() -> Arc<KindsHandle> {
        Arc::new(KindsHandle::new())
    }

    pub(super) fn set_off(&self) {
        *self.state.write().unwrap_or_else(|p| p.into_inner()) = State::Off;
    }

    /// Makes `runtime` the running one.
    pub fn set_ready(&self, runtime: Arc<KindsRuntime>) {
        *self.state.write().unwrap_or_else(|p| p.into_inner()) = State::Ready(runtime);
    }

    pub(super) fn runtime(&self) -> Option<Arc<KindsRuntime>> {
        match &*self.state.read().unwrap_or_else(|p| p.into_inner()) {
            State::Ready(runtime) => Some(Arc::clone(runtime)),
            _ => None,
        }
    }

    /// Reads the scripts again now (a save, enable, disable or delete has committed). A failure
    /// leaves the registries as they were and is retried by the service's next tick.
    pub async fn reload(&self) {
        if let Some(runtime) = self.runtime()
            && runtime.refresh().await.is_err()
        {
            runtime.mark_dirty();
        }
    }

    /// What is loaded and what was skipped, for the operator.
    #[must_use]
    pub fn status(&self) -> KindsStatus {
        self.runtime()
            .map_or_else(KindsStatus::default, |r| r.status())
    }
}

pub(super) fn handles() -> MutexGuard<'static, HashMap<String, Arc<KindsHandle>>> {
    static HANDLES: std::sync::OnceLock<Mutex<HashMap<String, Arc<KindsHandle>>>> =
        std::sync::OnceLock::new();
    HANDLES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The handle of `site`: the same one for the controllers and for `register`, on every build.
#[must_use]
pub fn handle(site: &str) -> Arc<KindsHandle> {
    Arc::clone(
        handles()
            .entry(site.to_owned())
            .or_insert_with(|| Arc::new(KindsHandle::new())),
    )
}

/// What is loaded, and what is not and why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KindsStatus {
    /// API scripts served.
    pub api_scripts: usize,
    /// Scheduled scripts planned.
    pub scheduled_scripts: usize,
    /// Records that were not loaded.
    pub skipped: Vec<Skipped>,
}

/// The API scripts as a function source.
#[derive(Default)]
struct ApiSource {
    declarations: RwLock<Vec<(FunctionDecl, Arc<dyn FunctionHandler>)>>,
}

impl FunctionDeclSource for ApiSource {
    fn declarations(&self) -> Vec<(FunctionDecl, Arc<dyn FunctionHandler>)> {
        self.declarations
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// The scheduled scripts as a schedule source.
#[derive(Default)]
struct SchedSource {
    planned: RwLock<Vec<DynamicSchedule>>,
}

impl ScheduleSource for SchedSource {
    fn schedules(&self) -> Vec<DynamicSchedule> {
        self.planned
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Run records into the documents: the run-time summary only, never a message or a value.
struct DocStore {
    source: Arc<dyn KindSource>,
    dirty: Arc<AtomicBool>,
}

impl KindStore for DocStore {
    /// The compile step puts the hash on the very document being saved; nothing to write.
    fn record_compile<'a>(
        &'a self,
        _kind: ScriptKind,
        _name: &'a str,
        _outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn record_run<'a>(&'a self, run: KindRun) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let line = if run.ok {
                String::new()
            } else {
                format!("{} ({} ms)", run.code, run.duration_ms)
            };
            match run.kind {
                ScriptKind::Scheduled => {
                    self.source
                        .summarise(
                            SCHEDULED_SCRIPT_DOCTYPE,
                            &run.script,
                            vec![("last_error", Value::Text(line))],
                            true,
                        )
                        .await
                }
                // One write per call would be a write per request: only a failure is recorded.
                ScriptKind::Api if !run.ok => {
                    self.source
                        .summarise(
                            API_SCRIPT_DOCTYPE,
                            &run.script,
                            vec![("last_error", Value::Text(line))],
                            false,
                        )
                        .await
                }
                ScriptKind::Api => Ok(()),
            }
        })
    }

    fn disable<'a>(
        &'a self,
        kind: ScriptKind,
        name: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let reason: String = reason.chars().take(500).collect();
            let done = self
                .source
                .summarise(
                    kind.doctype_static(),
                    name,
                    vec![
                        ("enabled", Value::Bool(false)),
                        ("disabled_reason", Value::Text(reason)),
                    ],
                    false,
                )
                .await;
            self.dirty.store(true, Ordering::Release);
            done
        })
    }
}

trait DoctypeStatic {
    fn doctype_static(self) -> &'static str;
}

impl DoctypeStatic for ScriptKind {
    fn doctype_static(self) -> &'static str {
        match self {
            ScriptKind::Api => API_SCRIPT_DOCTYPE,
            ScriptKind::Scheduled => SCHEDULED_SCRIPT_DOCTYPE,
        }
    }
}

struct Users(Arc<dyn KindSource>);

impl UserDirectory for Users {
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<Option<UserRecord>>> {
        self.0.user(name)
    }
}

/// What the last refresh loaded.
#[derive(Default)]
struct Loadout {
    api: Vec<ApiScript>,
    scheduled: Vec<ScheduledScript>,
    skipped: Vec<Skipped>,
    loaded_once: bool,
}

/// Everything a site's script kinds share once the module is ready.
pub struct KindsRuntime {
    site: SiteId,
    settings: Settings,
    limits: ScriptLimits,
    source: Arc<dyn KindSource>,
    deps: KindDeps,
    dirty: Arc<AtomicBool>,
    generation: AtomicU64,
    api: Arc<ApiSource>,
    sched: Arc<SchedSource>,
    functions: Mutex<Option<Arc<FunctionSources>>>,
    runner: RwLock<Option<Arc<ScheduledScriptRunner>>>,
    loadout: Mutex<Loadout>,
    /// How an elevated API script acts as the platform and is audited; `None` elevates nothing.
    elevation: Option<ElevationWiring>,
    /// Who holds a named permission (the save gate asks it); `None` answers no.
    permissions: Option<Arc<dyn PermissionCheck>>,
}

impl KindsRuntime {
    /// A runtime over its parts. `register` builds the production one; tests give it a source
    /// and an access factory over the in-memory engine.
    #[must_use]
    pub fn new(
        site: SiteId,
        settings: Settings,
        limits: ScriptLimits,
        source: Arc<dyn KindSource>,
        access: Arc<dyn ScriptAccessFactory>,
        clock: Arc<dyn Clock>,
        engine: spot_ts::ScriptEngine,
    ) -> KindsRuntime {
        let dirty = Arc::new(AtomicBool::new(false));
        let deps = KindDeps {
            engine,
            access,
            clock,
            store: Arc::new(DocStore {
                source: Arc::clone(&source),
                dirty: Arc::clone(&dirty),
            }),
            limits,
            currencies: None,
        };
        KindsRuntime {
            site,
            settings,
            limits,
            source,
            deps,
            dirty,
            generation: AtomicU64::new(0),
            api: Arc::new(ApiSource::default()),
            sched: Arc::new(SchedSource::default()),
            functions: Mutex::new(None),
            runner: RwLock::new(None),
            loadout: Mutex::new(Loadout::default()),
            elevation: None,
            permissions: None,
        }
    }

    /// The same runtime able to run the API scripts a record flags as elevated, audited through
    /// `wiring` (WP-EXT-06.T10). Without it a flagged script runs as its caller. Scheduled
    /// scripts are never elevated, whatever is wired.
    #[must_use]
    pub fn with_elevation(mut self, wiring: ElevationWiring) -> KindsRuntime {
        self.elevation = Some(wiring);
        self
    }

    /// The same runtime with the permission check the save gate of the two DocTypes asks.
    #[must_use]
    pub fn with_permissions(mut self, permissions: Arc<dyn PermissionCheck>) -> KindsRuntime {
        self.permissions = Some(permissions);
        self
    }

    /// Whether `inv`'s actor holds the named permission. `false` when no check is attached.
    pub async fn holds(&self, inv: &Invocation, permission: &str) -> bool {
        match &self.permissions {
            Some(check) => check.holds(inv, permission).await,
            None => false,
        }
    }

    pub(super) fn api_source(&self) -> Arc<dyn FunctionDeclSource> {
        Arc::clone(&self.api) as Arc<dyn FunctionDeclSource>
    }

    pub(super) fn schedule_source(&self) -> Arc<dyn ScheduleSource> {
        Arc::clone(&self.sched) as Arc<dyn ScheduleSource>
    }

    pub(super) fn attach_functions(&self, functions: Arc<FunctionSources>) {
        *self
            .functions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(functions);
    }

    /// Asks the service to read the scripts again at its next tick.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub(super) fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// How many refreshes have rebuilt a registry (a test reads it to see a change took effect).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn runner(&self) -> Option<Arc<ScheduledScriptRunner>> {
        self.runner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// What is loaded now.
    #[must_use]
    pub fn status(&self) -> KindsStatus {
        let loadout = self
            .loadout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        KindsStatus {
            api_scripts: loadout.api.len(),
            scheduled_scripts: loadout.scheduled.len(),
            skipped: loadout.skipped.clone(),
        }
    }

    /// Reads both DocTypes and rebuilds whichever registry changed. An unchanged list keeps its
    /// handlers and runner (compile caches and failure counters survive).
    ///
    /// # Errors
    ///
    /// `serverscript.store_failed` when the documents cannot be read; nothing changes then.
    pub async fn refresh(&self) -> AppResult<()> {
        let api = self.source.api_scripts(&self.limits).await?;
        let scheduled = self.source.scheduled_scripts(&self.limits).await?;
        let max = self.settings.max_scripts as usize;
        let mut skipped = api.skipped;
        skipped.extend(scheduled.skipped);

        let mut api_scripts = api.scripts;
        if api_scripts.len() > max {
            for extra in api_scripts.split_off(max) {
                skipped.push(Skipped {
                    doctype: API_SCRIPT_DOCTYPE,
                    name: extra.name,
                    reason: "over services.script_kinds.max_scripts".to_owned(),
                });
            }
        }
        // Two scripts on one function key disable both, and the status says so.
        let conflicts = ApiScriptRegistry::conflicts(&api_scripts);
        for conflict in &conflicts {
            for name in &conflict.scripts {
                skipped.push(Skipped {
                    doctype: API_SCRIPT_DOCTYPE,
                    name: name.clone(),
                    reason: format!(
                        "{}: `{}` is claimed by {} scripts; none is served",
                        spot_server_scripts::FUNCTION_CONFLICT,
                        conflict.key,
                        conflict.scripts.len()
                    ),
                });
            }
        }
        api_scripts.retain(|s| !conflicts.iter().any(|c| c.scripts.contains(&s.name)));

        let mut scheduled_scripts = scheduled.scripts;
        if scheduled_scripts.len() > max {
            for extra in scheduled_scripts.split_off(max) {
                skipped.push(Skipped {
                    doctype: SCHEDULED_SCRIPT_DOCTYPE,
                    name: extra.name,
                    reason: "over services.script_kinds.max_scripts".to_owned(),
                });
            }
        }

        let (api_changed, sched_changed) = {
            let loadout = self
                .loadout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                !loadout.loaded_once || loadout.api != api_scripts,
                !loadout.loaded_once || loadout.scheduled != scheduled_scripts,
            )
        };
        if api_changed {
            let per_minute = RateLimit {
                permits: self.settings.default_rate_limit_per_minute,
                per: Duration::from_secs(60),
            };
            let mut built =
                ApiScriptRegistry::build_with(&api_scripts, &self.deps, self.elevation.as_ref())?;
            for (decl, _) in &mut built {
                decl.limit = decl.limit.or(Some(per_minute));
            }
            *self
                .api
                .declarations
                .write()
                .unwrap_or_else(|p| p.into_inner()) = built;
            let functions = self
                .functions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(functions) = functions
                && let Err(conflicts) = functions.rebuild()
            {
                // A script shadowing a manifest function: the overlay that was serving stays.
                tracing::warn!(site = %self.site, %conflicts, "script functions were not reloaded");
            }
        }
        if sched_changed {
            let runner = Arc::new(ScheduledScriptRunner::new(
                self.site.clone(),
                scheduled_scripts.clone(),
                self.deps.clone(),
                Arc::new(Users(Arc::clone(&self.source))),
            ));
            let planned: Vec<DynamicSchedule> = runner
                .schedules()
                .into_iter()
                .map(|(schedule, policy, target)| DynamicSchedule {
                    service: SCRIPTING_APP.to_owned(),
                    schedule: Arc::new(schedule),
                    policy,
                    job: target.job,
                    args: target.args,
                })
                .collect();
            *self
                .sched
                .planned
                .write()
                .unwrap_or_else(|p| p.into_inner()) = planned;
            *self.runner.write().unwrap_or_else(|p| p.into_inner()) = Some(runner);
        }
        if api_changed || sched_changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        let mut loadout = self
            .loadout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loadout.api = api_scripts;
        loadout.scheduled = scheduled_scripts;
        loadout.skipped = skipped;
        loadout.loaded_once = true;
        Ok(())
    }

    /// Runs the scheduled script a job names, for the slot the job stands for.
    pub async fn run_job(&self, job: &spot_jobs::Job) -> HandlerResult {
        let Some(name) = job.args.get("script").and_then(Json::as_str) else {
            return HandlerResult::Fail("serverscript.record_invalid".to_owned());
        };
        let Some(runner) = self.runner() else {
            return HandlerResult::Retry("scripts are not loaded yet".to_owned());
        };
        let at = job.not_before.unix_timestamp();
        let slot = RunSlot {
            schedule: name.to_owned(),
            scheduled_for: jiff::Timestamp::from_second(at).unwrap_or(jiff::Timestamp::UNIX_EPOCH),
            deduplication_key: job
                .idempotency_key
                .clone()
                .unwrap_or_else(|| format!("{SCRIPTING_APP}:{name}:{at}")),
            slot_ref: SlotRef::new(SCRIPTING_APP, name),
        };
        match runner.run(name, &slot).await {
            Ok(_) => HandlerResult::Ok,
            Err(error) => {
                // Names and codes only. A failed script is never retried by the job engine; the
                // next slot is the next attempt.
                tracing::info!(
                    target: "spot::audit::script_kinds",
                    script = %name,
                    code = %error.code,
                    "scheduled script did not run"
                );
                HandlerResult::Fail(error.code.clone())
            }
        }
    }
}
