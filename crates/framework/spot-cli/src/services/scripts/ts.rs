//! TypeScript controllers and the installation of the controllers (WP-EXT-06.T4).

use super::*;

/// The longest controller module read.
pub(super) const MAX_MODULE_BYTES: u64 = 1024 * 1024;

/// The modules of one installed app, read from its directory: `controllers/invoice.ts` is the file
/// of that name under the app's folder. Nothing outside it is ever read: a path with a parent
/// step, a root, a drive or a symlink is refused as missing, and so is a file over 1 MiB.
pub struct DirModules {
    root: PathBuf,
}

impl DirModules {
    /// A source over the app directory `root`.
    #[must_use]
    pub fn new(root: PathBuf) -> DirModules {
        DirModules { root }
    }

    fn missing(app: &str, path: &str) -> AppError {
        AppError::new(
            ErrorClass::NotFound,
            spot_ts_controllers::MODULE_MISSING,
            format!("The installed bundle of `{app}` carries no controller module `{path}`."),
        )
    }
}

/// Whether `path` is a relative `.ts` path made of plain names.
pub(super) fn safe_module_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 200
        && path.ends_with(".ts")
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.starts_with('.'))
}

impl ModuleSource for DirModules {
    fn read(&self, app: &str, path: &str) -> AppResult<String> {
        if !safe_module_path(path) {
            return Err(Self::missing(app, path));
        }
        let mut file = self.root.clone();
        for part in path.split('/') {
            file.push(part);
            // A symlink is never followed, in any component.
            let meta = std::fs::symlink_metadata(&file).map_err(|_| Self::missing(app, path))?;
            if meta.file_type().is_symlink() {
                return Err(Self::missing(app, path));
            }
        }
        let meta = std::fs::metadata(&file).map_err(|_| Self::missing(app, path))?;
        if !meta.is_file() || meta.len() > MAX_MODULE_BYTES {
            return Err(Self::missing(app, path));
        }
        std::fs::read_to_string(&file).map_err(|_| Self::missing(app, path))
    }
}

/// A [`FieldView`] that asks the slot's runtime when it is called, so a TypeScript controller can
/// be built before the engine exists. While the slot is not ready it refuses.
pub struct SlotView {
    slot: Arc<ScriptSlot>,
}

impl FieldView for SlotView {
    fn readable<'a>(
        &'a self,
        inv: &'a Invocation,
        def: &'a DocTypeDef,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            match self.slot.state() {
                SlotState::Ready(runtime) => runtime.view.readable(inv, def, doc).await,
                SlotState::Off => Ok(doc.clone()),
                SlotState::Pending => Err(not_ready()),
            }
        })
    }
}

/// The factory that builds `ts:<path>` references of `app` from its installed directory `dir`,
/// running on the slot's engine under `limits` (use [`Settings::ts_limits`]).
#[must_use]
pub fn typescript_factory(
    slot: &Arc<ScriptSlot>,
    app: &str,
    dir: PathBuf,
    limits: TsLimits,
) -> TsControllerFactory {
    TsControllerFactory::new(
        app,
        slot.ts_engine().clone(),
        Arc::new(DirModules::new(dir)),
    )
    .with_limits(limits)
    .with_view(Arc::new(SlotView {
        slot: Arc::clone(slot),
    }))
}

/// Registers the Server Script controllers into `registry`: the administration controller for
/// `server_script` and one [`ScriptHub`] for each of `doctypes` that is [`is_scriptable`]. Call
/// it once per site build, with the DocTypes of the metadata the engine will serve, before the
/// registry is installed.
///
/// The admin controller records compile results through a store that writes nothing (the compile
/// step puts the hash on the document being saved), so it needs no engine.
pub fn register_controllers<'a>(
    registry: &mut ControllerRegistry,
    site: &str,
    settings: &Settings,
    doctypes: impl IntoIterator<Item = &'a str>,
) {
    let slot = slot(site);
    register_admin(registry, site);
    let budget = settings.step_budget();
    for doctype in doctypes {
        if is_scriptable(doctype) {
            registry.register(
                APP,
                Arc::new(ScriptHub::new(doctype, Arc::clone(&slot), budget)),
                ControllerSource::TypeScript,
            );
        }
    }
}

/// Registers only the administration controller of `server_script` (compile on save, the target
/// check, the cache bump and the elevation save gate), not the hubs. The host calls this for
/// every site, since none of it depends on the module having finished assembling; the hubs
/// ([`register_controllers`]) refuse writes until it has.
pub fn register_admin(registry: &mut ControllerRegistry, site: &str) {
    let slot = slot(site);
    registry.register(
        APP,
        Arc::new(ScriptAdmin::new(Arc::clone(&slot), Arc::new(NoStore))),
        ControllerSource::TypeScript,
    );
}

/// The store the admin controller records compile results through: it keeps nothing.
pub(super) struct NoStore;

impl ScriptStore for NoStore {
    fn enabled_for<'a>(&'a self, _doctype: &'a str) -> BoxFuture<'a, AppResult<Vec<ServerScript>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn record_compile<'a>(
        &'a self,
        _doctype: &'a str,
        _name: &'a str,
        _outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn record_run<'a>(&'a self, _run: ScriptRun) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn disable<'a>(
        &'a self,
        _doctype: &'a str,
        _name: &'a str,
        _reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
