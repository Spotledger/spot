//! Hot reload for `spot serve` in development (WP-DX-02.T2): the pipeline of `spot-devserver`
//! (WP-DX-02.T1) run against the real site.
//!
//! On a developer's machine the module watches the app directories the site was built from and
//! applies a change without a restart, then tells connected clients:
//!
//! - **DocType edits** are re-read, built and validated as a whole, and swapped into the engine,
//!   the permission snapshot and the served metadata ([`SiteMetaStep`]); a file that does not
//!   build leaves the previous metadata serving and reports the problems.
//! - **Script edits** are transpiled and type-checked against the live declarations, then handed
//!   to the site's TypeScript engine ([`ScriptRuntime`]); a call in flight finishes on the old
//!   version.
//! - **View edits** are parsed and validated against what the engine serves.
//! - **Component builds** are validated and swapped into the WASM app runtime
//!   ([`ComponentRuntime`]); a failed validation keeps the old component.
//! - **`dev.reloaded`** reaches connected clients on the site room of the realtime hub, with
//!   identifiers and outcomes only.
//!
//! # Off unless asked, and loopback only
//!
//! The module registers nothing unless `[services.devserver] enabled = true`, so a site that does
//! not ask for it has no watcher thread and no listener. Enabled, it refuses to start unless
//! every `server.bind` address and the status endpoint are loopback ([`guard::resolve`]).
//!
//! # Safety
//!
//! - Only the directories of apps installed from a local path are watched, platform app excluded.
//! - A symlink that leaves the app directory is not followed ([`Guarded`]): the change is ignored
//!   and the record says so.
//! - A file that is not a DocType, view, script, component, fixture or asset is shown as ignored.
//! - The status endpoint (`GET /dev/status`) is read-only, on loopback, and carries no file
//!   content.
//!
//! # The runtimes
//!
//! Enabled and loopback, the module provides the script runtime and the component runtime as
//! ports ([`runtimes`]): a script swap compiles on the site's TypeScript engine and the next call
//! runs the new text, a component swap validates the binary and changes the bundle hash. A host
//! that is not a development host provides neither port, and the module does not start.
//!
//! # What this module does not do
//!
//! It does **not** reset or reseed a real site (`SiteControl` of the library stays a scratch-site
//! tool for `spot dev check`): resetting a database is destructive.
//!
//! # Permission files
//!
//! An edit of `permissions/*.toml` re-reads the site's merged permission model, checks it against
//! the metadata being served and swaps it in atomically ([`permission_reload`]); a file that does
//! not build keeps the old model and reports the loader's sentence under the `devserver.metadata`
//! code. Only a development host builds this step.

#[path = "devserver/guard.rs"]
pub mod guard;
#[path = "devserver/permission_reload.rs"]
pub mod permission_reload;
#[path = "devserver/runtimes.rs"]
pub mod runtimes;
#[path = "devserver/seams.rs"]
pub mod seams;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use spot_core::{Clock, SiteId, SystemClock};
use spot_devserver::reload::{
    BuildCommand, CommandBuildRunner, ComponentStep, DeclarationNameChecker, DefinitionsSource,
    LiveDtsProvider, Reloader, ScriptStep, StepName, ViewStep,
};
use spot_devserver::status::{StatusServer, StatusStore};
use spot_devserver::watch::{NotifyEventSource, Watcher, WatcherConfig};
use spot_devserver::{Classifier, DevServer};
use tokio::sync::watch;

pub use guard::{Resolved, Settings, resolve, stays_inside};
pub use permission_reload::{
    LiveTarget, MetaOrPermissions, PermissionBuild, PermissionModels, PermissionReload,
    PermissionTarget, SiteModels, is_permission_file,
};
pub use runtimes::{DevComponents, DevScripts, Runtimes, provide_runtimes};
pub use seams::{
    ComponentRuntime, Guarded, HubNotifier, NoteStep, RuntimeComponentHost, RuntimeScriptHost,
    ScriptRuntime, SiteDefinitions, SiteMetaStep,
};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the realtime hub and the metadata services, before the process modules.
pub const ORDER: u32 = 1060;

/// The app directories hot reload watches: the local directories of the apps the site has
/// installed (the embedded platform app has none), or every listed directory for a host-wide
/// site.
fn app_dirs(env: &crate::services::SiteEnv<'_>) -> Result<Vec<std::path::PathBuf>> {
    let mut dirs = Vec::new();
    if env.site.apps.is_empty() {
        for app in &env.loaded.common.apps {
            let dir = env.root.join(app);
            if dir.join("doctypes").is_dir() {
                dirs.push(dir);
            }
        }
    } else {
        for name in &env.site.apps {
            if name == crate::appdir::PLATFORM {
                continue;
            }
            dirs.push(crate::apps::resolve(env.root, env.loaded, name)?.dir);
        }
    }
    Ok(dirs)
}

/// Registers hot reload when `[services.devserver] enabled = true`.
///
/// # Errors
///
/// A start that is not safe for development (a non-loopback address), a bad setting, or an app
/// directory that cannot be watched.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("devserver")?;
    if !settings.enabled || !ctx.assembling() {
        // Off registers nothing: no watcher thread, no listener. A reload keeps what runs.
        return Ok(());
    }
    let env = *ctx.env()?;
    let binds: Vec<_> = env.loaded.server.bind.clone();
    let resolved = resolve(&settings, &binds).map_err(Error)?;
    let site_id = SiteId::new(&env.site.site).map_err(|e| Error(format!("devserver: {e}")))?;
    let live = Arc::clone(env.live);
    let definitions: Arc<dyn DefinitionsSource> = Arc::new(SiteDefinitions::new(Arc::clone(&live)));
    // Only a host that passed the loopback check above provides the two runtimes; a non-dev host
    // provides neither and never gets here.
    let engine = crate::services::scripts::slot(&env.site.site)
        .ts_engine()
        .clone();
    provide_runtimes(ctx, engine)?;
    let scripts = RuntimeScriptHost::new(ctx.port::<dyn ScriptRuntime>());
    let components = RuntimeComponentHost::new(ctx.port::<dyn ComponentRuntime>());
    let scripts: Arc<dyn spot_devserver::reload::ScriptHost> = Arc::new(scripts);
    let components: Arc<dyn spot_devserver::reload::ComponentHost> = Arc::new(components);
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| Error("devserver: hot reload needs the host's tokio runtime".to_owned()))?;
    let status = Arc::new(StatusStore::new(settings.history));
    let notifier = Arc::new(HubNotifier::new(Arc::clone(&live), site_id));

    let mut servers = Vec::new();
    for dir in app_dirs(&env)? {
        let app_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app")
            .to_owned();
        let utf8 = seams::utf8(dir.clone()).map_err(Error)?;
        let build = settings.build.as_ref().map(|program| {
            settings
                .build_args
                .iter()
                .fold(BuildCommand::new(program.clone(), utf8.clone()), |c, a| {
                    c.arg(a.clone())
                })
        });
        let meta_step = Guarded::new(
            SiteMetaStep::new(
                env.root.to_path_buf(),
                env.loaded.clone(),
                env.site.clone(),
                Arc::clone(&live),
                runtime.clone(),
            ),
            dir.clone(),
        );
        let view_step = Guarded::new(
            ViewStep::new(utf8.join("views"), Arc::clone(&definitions)),
            dir.clone(),
        );
        let script_step = Guarded::new(
            ScriptStep::new(
                utf8.join("ts"),
                Arc::new(LiveDtsProvider::new(Arc::clone(&definitions))),
                Arc::new(DeclarationNameChecker),
                Arc::clone(&scripts),
            ),
            dir.clone(),
        );
        let component_step = Guarded::new(
            ComponentStep::new(
                app_name,
                build,
                Arc::new(CommandBuildRunner),
                Arc::clone(&components),
                utf8.join(&settings.artifact),
            ),
            dir.clone(),
        );
        // Permission files arrive as metadata changes; the wrapper applies them to the permission
        // model and hands every other change to the metadata step unchanged.
        let meta_step = MetaOrPermissions::new(
            Arc::new(meta_step),
            PermissionReload::new(
                Arc::new(SiteModels::new(
                    env.root.to_path_buf(),
                    env.loaded.clone(),
                    env.site.clone(),
                )),
                Arc::new(LiveTarget::new(Arc::clone(&live), runtime.clone())),
            ),
        );
        let reloader = Reloader::new(settings.history)
            .with_step(Arc::new(meta_step))
            .with_step(Arc::new(view_step))
            .with_step(Arc::new(script_step))
            .with_step(Arc::new(component_step))
            .with_step(Arc::new(NoteStep::new(
                StepName::Fixtures,
                "fixture change noted; the dev server never reseeds a real site",
            )))
            .with_step(Arc::new(NoteStep::new(
                StepName::Assets,
                "asset change noted; clients refetch it",
            )))
            .with_step(Arc::new(NoteStep::new(
                StepName::Unknown,
                "ignored: not a DocType, view, script, component, fixture or asset",
            )))
            .with_notifier(notifier.clone());
        let source = NotifyEventSource::new().map_err(|e| Error(format!("devserver: {e}")))?;
        let watcher = Watcher::new(
            source,
            WatcherConfig {
                debounce: resolved.debounce,
                extra_ignored: Vec::new(),
            },
        );
        servers.push(DevServer::new(
            utf8,
            Classifier::new(),
            watcher,
            Arc::new(reloader),
            Arc::clone(&status),
        ));
    }
    ctx.add(Arc::new(DevService {
        servers: Mutex::new(Some(servers)),
        status,
        status_addr: resolved.status_addr,
        poll: resolved.poll,
        stopping: Arc::new(AtomicBool::new(false)),
    }));
    Ok(())
}

/// The running watcher: one blocking loop drains every app's watcher and runs its pipeline, and
/// one thread serves the status endpoint. Both end when the site stops.
struct DevService {
    servers: Mutex<Option<Vec<DevServer<NotifyEventSource>>>>,
    status: Arc<StatusStore>,
    status_addr: std::net::SocketAddr,
    poll: Duration,
    stopping: Arc<AtomicBool>,
}

impl Service for DevService {
    fn name(&self) -> &'static str {
        "devserver"
    }

    fn level(&self) -> Level {
        // A broken dev loop must never take a development site out of rotation.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let taken = self
                .servers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            let Some(mut servers) = taken else {
                return Err("the dev server was already started".to_owned());
            };
            for server in &mut servers {
                server.watch().map_err(|e| e.to_string())?;
            }
            let endpoint = StatusServer::new(Arc::clone(&self.status), self.status_addr);
            let flag = Arc::clone(&self.stopping);
            let status_task = tokio::task::spawn_blocking(move || {
                endpoint.serve_until(&flag).map_err(|e| e.to_string())
            });
            let poll = self.poll;
            let flag = Arc::clone(&self.stopping);
            let stop_for_loop = stop.clone();
            let loop_task = tokio::task::spawn_blocking(move || {
                let clock = SystemClock::new();
                while !*stop_for_loop.borrow() && !flag.load(Ordering::Relaxed) {
                    for server in &mut servers {
                        let _ = server.poll_once(&clock as &dyn Clock);
                    }
                    std::thread::sleep(poll);
                }
            });
            let mut stop = stop;
            let _ = stop.wait_for(|stopped| *stopped).await;
            self.stopping.store(true, Ordering::Relaxed);
            let _ = loop_task.await;
            match status_task.await {
                Ok(result) => result,
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        self.stopping.store(true, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }
}

/// Turns the app directory of a site into the UTF-8 path the library wants (test helper).
#[must_use]
pub fn to_utf8(path: std::path::PathBuf) -> Option<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path).ok()
}
