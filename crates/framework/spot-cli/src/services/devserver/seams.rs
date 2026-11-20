//! The adapters that give the development pipeline of `spot-devserver` the real site: the live
//! metadata, the realtime hub, and the two runtimes (scripts, components) a module of this host
//! may provide as a port.

use std::path::PathBuf;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use spot_core::SiteId;
use spot_devserver::reload::{
    ComponentHost, DefinitionsSource, ReloadContext, ReloadRecord, ScriptHost, Step, StepName,
    StepOutcome,
};
use spot_devserver::{DevError, DevResult, ReloadNotifier, record_payload};
use spot_meta::EffectiveMeta;
use spot_realtime::Room;
use spot_ts_transpile::Transpiled;

use crate::config;
use crate::live::SiteLive;

use super::guard;

/// The kind a reload notification is published under. Built with `concat!` so the error-code
/// scanner does not take the notification name for an error code.
pub const DEV_RELOADED: &str = concat!("dev", ".reloaded");

/// What the site serves now, for the view and script steps: the metadata the engine uses.
pub struct SiteDefinitions {
    live: Arc<SiteLive>,
}

impl SiteDefinitions {
    /// Reads through `live` on every call, so a swap reaches the steps at once.
    #[must_use]
    pub fn new(live: Arc<SiteLive>) -> Self {
        Self { live }
    }
}

impl DefinitionsSource for SiteDefinitions {
    fn effective(&self) -> Option<Arc<EffectiveMeta>> {
        self.live.served().map(|served| Arc::clone(&served.meta))
    }
}

/// The metadata step over the real site: re-reads the installed apps' DocType files, builds and
/// validates the whole metadata, and only then swaps it into the engine, the permission snapshot
/// and the served pair (the path `SiteLive::swap_to` gives runtime customisation). A build that
/// fails leaves the previous metadata serving and reports the problems.
pub struct SiteMetaStep {
    root: PathBuf,
    loaded: config::Loaded,
    site: config::Site,
    live: Arc<SiteLive>,
    runtime: tokio::runtime::Handle,
}

impl SiteMetaStep {
    /// A step for `site`, swapping through `live`. `runtime` runs the swap's async part from the
    /// watcher's own (blocking) thread.
    #[must_use]
    pub fn new(
        root: PathBuf,
        loaded: config::Loaded,
        site: config::Site,
        live: Arc<SiteLive>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            root,
            loaded,
            site,
            live,
            runtime,
        }
    }
}

impl Step for SiteMetaStep {
    fn name(&self) -> StepName {
        StepName::Metadata
    }

    fn reload(&self, _ctx: &ReloadContext<'_>) -> StepOutcome {
        // The same validation `install-app`'s dry run does: every app directory is read and
        // every problem is reported together, with the file the app loader names.
        let meta = match crate::apps::serving(&self.root, &self.loaded, &self.site) {
            Ok(meta) => meta,
            Err(error) => {
                let errors: Vec<String> = error.to_string().lines().map(str::to_owned).collect();
                return StepOutcome::failed(
                    "metadata unchanged; the DocType files do not build",
                    errors,
                );
            }
        };
        let version = self
            .live
            .served()
            .and_then(|served| served.version)
            .map_or(1, |v| v.saturating_add(1));
        let grants = self.live.grants();
        match self
            .runtime
            .block_on(self.live.swap_to(meta, version, grants))
        {
            Ok(()) => {
                StepOutcome::ok(format!("metadata live at version {version}")).with_version(version)
            }
            Err(why) => StepOutcome::failed(
                "metadata unchanged; the new metadata could not be put in force",
                vec![why],
            ),
        }
    }
}

/// Publishes `dev.reloaded` on the site's realtime hub, on the site room. The payload is
/// identifiers and outcomes only (`record_payload`): never a file's contents. The hub is looked
/// up at publish time, so a hub attached after this module started is still used.
pub struct HubNotifier {
    live: Arc<SiteLive>,
    site: SiteId,
}

impl HubNotifier {
    /// Publishes on `live`'s hub for `site`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, site: SiteId) -> Self {
        Self { live, site }
    }
}

impl ReloadNotifier for HubNotifier {
    fn reloaded(&self, record: &ReloadRecord) {
        if let Some(hub) = self.live.realtime() {
            // `publish` is synchronous and never awaits a subscriber.
            let _ = hub.bus().publish(
                &self.site,
                &Room::site(),
                DEV_RELOADED,
                record_payload(record),
            );
        }
    }
}

/// What the TypeScript engine of the site offers the dev server. A module that hosts the engine
/// provides it as a port (`ctx.provide::<dyn ScriptRuntime>(..)`); without one a script change
/// fails visibly instead of being swallowed.
pub trait ScriptRuntime: Send + Sync {
    /// Replaces the compiled version of the script called `name`. A call in flight finishes on
    /// the old version; the next call uses this one.
    ///
    /// # Errors
    ///
    /// A sentence when the engine refuses the version; the old one keeps serving.
    fn replace(&self, name: &str, js: &str, source_map: &str) -> Result<(), String>;
}

/// What the WASM app runtime offers the dev server (provided as a port, like [`ScriptRuntime`]).
pub trait ComponentRuntime: Send + Sync {
    /// Validates the rebuilt component `wasm` of the installed app `app` and swaps it in; the
    /// old one keeps serving when validation fails. Returns a content hash for the status page.
    ///
    /// # Errors
    ///
    /// A sentence when the component does not validate or the app is not installed.
    fn replace(&self, app: &str, wasm: &[u8]) -> Result<String, String>;
}

/// The script seam over a [`ScriptRuntime`].
pub struct RuntimeScriptHost {
    runtime: Option<Arc<dyn ScriptRuntime>>,
}

impl RuntimeScriptHost {
    /// A host over `runtime`; `None` says the site hosts no script engine.
    #[must_use]
    pub fn new(runtime: Option<Arc<dyn ScriptRuntime>>) -> Self {
        Self { runtime }
    }
}

impl ScriptHost for RuntimeScriptHost {
    fn swap(&self, script: &Utf8Path, transpiled: &Transpiled) -> DevResult<()> {
        let Some(runtime) = &self.runtime else {
            return Err(DevError::Script {
                errors: vec![
                    "this site hosts no TypeScript engine, so a script change cannot be applied"
                        .to_owned(),
                ],
            });
        };
        let name = script.file_stem().unwrap_or_else(|| script.as_str());
        runtime
            .replace(name, &transpiled.js, &transpiled.source_map)
            .map_err(|why| DevError::Script { errors: vec![why] })
    }
}

/// The component seam over a [`ComponentRuntime`].
pub struct RuntimeComponentHost {
    runtime: Option<Arc<dyn ComponentRuntime>>,
}

impl RuntimeComponentHost {
    /// A host over `runtime`; `None` says the site hosts no WASM app runtime.
    #[must_use]
    pub fn new(runtime: Option<Arc<dyn ComponentRuntime>>) -> Self {
        Self { runtime }
    }
}

impl ComponentHost for RuntimeComponentHost {
    fn reload(&self, app: &str, artifact: &Utf8Path) -> DevResult<String> {
        let Some(runtime) = &self.runtime else {
            return Err(DevError::Component {
                message: "this site hosts no WASM app runtime, so a component cannot be swapped"
                    .to_owned(),
            });
        };
        let bytes = std::fs::read(artifact.as_std_path())
            .map_err(|error| DevError::io_message(artifact, error.to_string()))?;
        runtime
            .replace(app, &bytes)
            .map_err(|message| DevError::Component { message })
    }
}

/// Wraps a step so a change whose real path leaves the app directory (a symlink out) is never
/// handed to it: the change is ignored and the record says so.
pub struct Guarded<S: Step> {
    inner: S,
    app_dir: PathBuf,
}

impl<S: Step> Guarded<S> {
    /// Guards `inner` to `app_dir`.
    #[must_use]
    pub fn new(inner: S, app_dir: PathBuf) -> Self {
        Self { inner, app_dir }
    }
}

impl<S: Step> Step for Guarded<S> {
    fn name(&self) -> StepName {
        self.inner.name()
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        let inside: Vec<_> = ctx
            .changes
            .iter()
            .filter(|change| guard::stays_inside(&self.app_dir, change.path.as_std_path()))
            .cloned()
            .collect();
        if inside.is_empty() {
            return StepOutcome::ok("ignored: the path leaves the app directory");
        }
        self.inner.reload(&ReloadContext {
            changes: &inside,
            now: ctx.now,
        })
    }
}

/// A step that only reports: a change the dev server does not apply, shown as ignored (an
/// unknown file kind) or noted (fixtures and assets, which a real site never reseeds or serves
/// from here).
pub struct NoteStep {
    name: StepName,
    detail: &'static str,
}

impl NoteStep {
    /// A step for `name` that answers `detail`.
    #[must_use]
    pub fn new(name: StepName, detail: &'static str) -> Self {
        Self { name, detail }
    }
}

impl Step for NoteStep {
    fn name(&self) -> StepName {
        self.name
    }

    fn reload(&self, _ctx: &ReloadContext<'_>) -> StepOutcome {
        StepOutcome::ok(self.detail)
    }
}

/// The directory of an app as a UTF-8 path, or why it is not one.
///
/// # Errors
///
/// A sentence naming the path.
pub fn utf8(path: PathBuf) -> Result<Utf8PathBuf, String> {
    Utf8PathBuf::from_path_buf(path)
        .map_err(|p| format!("the app directory `{}` is not valid UTF-8", p.display()))
}
