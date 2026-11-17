//! The per-site slot and the runtime the hubs share (WP-EXT-06.T4).

use super::*;


/// What a site's scripts are doing right now.
#[derive(Clone)]
pub enum SlotState {
    /// Assembly has not finished: scriptable writes are refused.
    Pending,
    /// `enabled = false` (or the site is not assembling): scripts do not run.
    Off,
    /// Scripts run.
    Ready(Arc<Runtime>),
}

/// The late-bound handle between the controllers the host installs **before** the engine exists
/// and the runtime this module builds **after** it: the controllers hold the slot, `register`
/// fills it. The two engines are built with the slot because they need nothing from the site.
pub struct ScriptSlot {
    script_engine: ScriptEngine,
    ts_engine: ScriptEngine,
    state: RwLock<SlotState>,
}

impl ScriptSlot {
    fn new() -> ScriptSlot {
        ScriptSlot {
            script_engine: ScriptEngine::new(EngineConfig {
                surface: host_surface(),
                ..EngineConfig::default()
            }),
            ts_engine: ScriptEngine::new(EngineConfig {
                surface: ts_host_surface(),
                ..EngineConfig::default()
            }),
            state: RwLock::new(SlotState::Pending),
        }
    }

    /// A slot not shared with any site, for tests and tools.
    #[must_use]
    pub fn detached() -> Arc<ScriptSlot> {
        Arc::new(ScriptSlot::new())
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> SlotState {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sets the state.
    pub fn set(&self, state: SlotState) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }

    /// The engine Server Scripts run on.
    #[must_use]
    pub fn script_engine(&self) -> &ScriptEngine {
        &self.script_engine
    }

    /// The engine TypeScript controllers run on.
    #[must_use]
    pub fn ts_engine(&self) -> &ScriptEngine {
        &self.ts_engine
    }
}

pub(super) fn slots() -> MutexGuard<'static, HashMap<String, Arc<ScriptSlot>>> {
    static SLOTS: std::sync::OnceLock<Mutex<HashMap<String, Arc<ScriptSlot>>>> =
        std::sync::OnceLock::new();
    SLOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The slot of `site`: the same one for the controllers the host installs and for `register`, on
/// every build of the site (a reload keeps the engines and their compile cache).
#[must_use]
pub fn slot(site: &str) -> Arc<ScriptSlot> {
    Arc::clone(
        slots()
            .entry(site.to_owned())
            .or_insert_with(|| Arc::new(ScriptSlot::new())),
    )
}

/// One loaded script list of a DocType.
pub(super) struct Cached {
    loaded: Instant,
    generation: u64,
    controller: Arc<ScriptController>,
}

/// Everything the hubs of a site share once the module is ready.
pub struct Runtime {
    pub(super) settings: Settings,
    pub(super) engine: ScriptEngine,
    pub(super) store: Arc<dyn ScriptStore>,
    pub(super) view: Arc<dyn FieldView>,
    pub(super) caller: Arc<dyn MethodCaller>,
    pub(super) limits: ScriptLimits,
    pub(super) log: Arc<ScriptLog>,
    pub(super) generation: Arc<AtomicU64>,
    pub(super) cache: Mutex<HashMap<String, Cached>>,
}

impl Runtime {
    /// A runtime over its parts. `register` builds the production one; tests give it a fake
    /// store, a view and a caller.
    pub fn new(
        settings: Settings,
        engine: ScriptEngine,
        store: Arc<dyn ScriptStore>,
        view: Arc<dyn FieldView>,
        caller: Arc<dyn MethodCaller>,
    ) -> Runtime {
        let limits = settings.script_limits();
        let log = Arc::new(ScriptLog::new(settings.log_capacity));
        Runtime {
            settings,
            engine,
            store,
            view,
            caller,
            limits,
            log,
            generation: Arc::new(AtomicU64::new(0)),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The settings the runtime runs under.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The run log (in memory; a record names the script, the event and a code, never a value).
    #[must_use]
    pub fn log(&self) -> &Arc<ScriptLog> {
        &self.log
    }

    /// Makes every hub read its scripts again on its next step.
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The counter a store bumps to invalidate (the adapter holds a clone).
    #[must_use]
    pub fn generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<String, Cached>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The controller running `doctype`'s enabled scripts: the cached one while its generation
    /// and age hold, else a fresh read of the store. An unchanged list keeps the old controller,
    /// so its failure counters survive a refresh.
    ///
    /// # Errors
    ///
    /// The store could not be read (`serverscript.store_failed`), or the scripts cannot be
    /// built into steps.
    pub async fn controller_for(&self, doctype: &str) -> AppResult<Arc<ScriptController>> {
        let generation = self.generation.load(Ordering::Acquire);
        let fresh = Duration::from_secs(self.settings.refresh_secs);
        if let Some(cached) = self.cache().get(doctype)
            && cached.generation == generation
            && cached.loaded.elapsed() < fresh
        {
            return Ok(Arc::clone(&cached.controller));
        }
        let scripts = self.store.enabled_for(doctype).await?;
        let previous = self
            .cache()
            .get(doctype)
            .map(|cached| Arc::clone(&cached.controller));
        let controller = match previous {
            Some(previous) if previous.scripts() == scripts => previous,
            _ => Arc::new(
                ScriptController::new(
                    doctype,
                    scripts,
                    self.engine.clone(),
                    Arc::clone(&self.store),
                )?
                .with_view(Arc::clone(&self.view))
                .with_limits(self.limits)
                .with_log(Arc::clone(&self.log))
                .with_method_caller(Arc::clone(&self.caller)),
            ),
        };
        self.cache().insert(
            doctype.to_owned(),
            Cached {
                loaded: Instant::now(),
                generation,
                controller: Arc::clone(&controller),
            },
        );
        Ok(controller)
    }
}

