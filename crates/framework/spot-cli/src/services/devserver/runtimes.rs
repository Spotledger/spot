//! The two runtimes hot reload swaps into (WP-DX-02.T3): the script runtime over the site's
//! TypeScript engine and the component runtime that keeps the shell bundle current.
//!
//! Both exist **only on a development host**: [`provide_runtimes`] is called by the dev server
//! module after it has checked that every listener is loopback, so a host that did not ask for
//! hot reload (or whose addresses are not loopback) provides neither port and the dev server
//! does not start. Nothing here reads a file or the clock; the dev server hands over bytes.
//!
//! # Scripts
//!
//! [`DevScripts`] compiles the transpiled text on the site's TypeScript engine under a new
//! [`ScriptId`] version for every successful swap and keeps the newest [`Compiled`] per script
//! name. A call in flight holds the `Arc` it started with and finishes on it; the next
//! [`DevScripts::call`] looks the name up again and runs the new text. A text that does not
//! compile is refused with a sentence (the script's stable code, never source text) and the
//! previous version keeps serving.
//!
//! # Components
//!
//! [`DevComponents`] validates a rebuilt WebAssembly binary (the magic number and a core module
//! or component version header, a size ceiling) and swaps it in as the current shell bundle of
//! the app. The content hash (SHA-256, lower-case hex) is what the status page shows, so an edit
//! that changes the component changes the hash.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use spot_ts::{Compiled, HostBridge, Limits, ScriptEngine, ScriptId};

use super::seams::{ComponentRuntime, ScriptRuntime};
use crate::Result;
use crate::services::SiteCtx;

/// The app name dev scripts are compiled under in the engine's cache.
pub const DEV_APP: &str = "dev_scripts";

/// The longest script text taken, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 1024 * 1024;

/// The longest component taken, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;

/// The longest script or app name taken.
const MAX_NAME: usize = 120;

/// Whether `name` is a plain script or app name: ASCII letters, digits, `_`, `-`, `.`, and `/`
/// between parts; no empty part, no `..`, no leading dot.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The script runtime of a development site.
pub struct DevScripts {
    engine: ScriptEngine,
    current: Mutex<BTreeMap<String, (u64, Arc<Compiled>)>>,
    generation: AtomicU64,
}

impl DevScripts {
    /// A runtime compiling on `engine`.
    #[must_use]
    pub fn new(engine: ScriptEngine) -> Self {
        Self {
            engine,
            current: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// The newest compiled version of the script `name`, or `None` before its first swap.
    #[must_use]
    pub fn current(&self, name: &str) -> Option<Arc<Compiled>> {
        lock(&self.current)
            .get(name)
            .map(|(_, compiled)| Arc::clone(compiled))
    }

    /// The version number of the newest swap of `name` (1 for the first).
    #[must_use]
    pub fn version(&self, name: &str) -> Option<u64> {
        lock(&self.current).get(name).map(|(version, _)| *version)
    }

    /// The names that have a swapped-in version, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        lock(&self.current).keys().cloned().collect()
    }

    /// Runs the exported function `entry` of the newest version of `name`.
    ///
    /// The version is looked up when the call starts; a swap during the call does not change the
    /// text that call runs.
    ///
    /// # Errors
    ///
    /// A sentence: the script has never been swapped in, or the engine's stable code (never
    /// script text or arguments).
    pub async fn call(
        &self,
        name: &str,
        entry: &str,
        args: &Json,
        bridge: Arc<dyn HostBridge>,
        limits: Limits,
    ) -> std::result::Result<Json, String> {
        let Some(compiled) = self.current(name) else {
            return Err(format!(
                "script `{name}` has not been loaded by the dev server"
            ));
        };
        self.engine
            .run(&compiled, entry, args, bridge, limits)
            .await
            .map_err(|error| format!("script `{name}` failed ({})", error.code()))
    }
}

impl ScriptRuntime for DevScripts {
    fn replace(&self, name: &str, js: &str, source_map: &str) -> std::result::Result<(), String> {
        if !plain_name(name) {
            return Err(
                "a script name must be plain letters, digits, `_`, `-`, `.` and `/`".to_owned(),
            );
        }
        if js.len() > MAX_SCRIPT_BYTES {
            return Err(format!(
                "script `{name}` is larger than {MAX_SCRIPT_BYTES} bytes"
            ));
        }
        let version = self
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let map = if source_map.trim().is_empty() {
            None
        } else {
            Some(source_map)
        };
        let compiled = self
            .engine
            .compile(js, ScriptId::new(DEV_APP, name, version), map)
            .map_err(|error| {
                format!(
                    "script `{name}` does not compile ({}); the previous version keeps serving",
                    error.code()
                )
            })?;
        lock(&self.current).insert(name.to_owned(), (version, compiled));
        Ok(())
    }
}

/// A validated shell bundle.
struct Bundle {
    hash: String,
    bytes: Arc<Vec<u8>>,
}

/// The component runtime of a development site: the current shell bundle per app.
#[derive(Default)]
pub struct DevComponents {
    bundles: Mutex<BTreeMap<String, Bundle>>,
}

/// The first eight bytes of a core module: `\0asm` and version 1.
const CORE_HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
/// The first eight bytes of a component: `\0asm`, version 0x0d, layer 1.
const COMPONENT_HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];

impl DevComponents {
    /// An empty runtime.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The content hash of the current bundle of `app`.
    #[must_use]
    pub fn bundle_hash(&self, app: &str) -> Option<String> {
        lock(&self.bundles).get(app).map(|b| b.hash.clone())
    }

    /// The bytes of the current bundle of `app`.
    #[must_use]
    pub fn bundle(&self, app: &str) -> Option<Arc<Vec<u8>>> {
        lock(&self.bundles).get(app).map(|b| Arc::clone(&b.bytes))
    }
}

impl ComponentRuntime for DevComponents {
    fn replace(&self, app: &str, wasm: &[u8]) -> std::result::Result<String, String> {
        if !plain_name(app) {
            return Err(
                "an app name must be plain letters, digits, `_`, `-`, `.` and `/`".to_owned(),
            );
        }
        if wasm.len() > MAX_COMPONENT_BYTES {
            return Err(format!(
                "the component of `{app}` is larger than {MAX_COMPONENT_BYTES} bytes"
            ));
        }
        let header = wasm.get(..8).unwrap_or(&[]);
        if header != CORE_HEADER && header != COMPONENT_HEADER {
            return Err(format!(
                "the build output of `{app}` is not a WebAssembly module or component; the old bundle keeps serving"
            ));
        }
        let hash = format!("{:x}", Sha256::digest(wasm));
        lock(&self.bundles).insert(
            app.to_owned(),
            Bundle {
                hash: hash.clone(),
                bytes: Arc::new(wasm.to_vec()),
            },
        );
        Ok(hash)
    }
}

/// What [`provide_runtimes`] built.
pub struct Runtimes {
    /// The script runtime.
    pub scripts: Arc<DevScripts>,
    /// The component runtime.
    pub components: Arc<DevComponents>,
}

/// Builds both runtimes over `engine` and provides them as ports (`dyn ScriptRuntime`,
/// `dyn ComponentRuntime`, and the concrete [`DevScripts`] and [`DevComponents`] for modules that
/// read the current versions).
///
/// Call it only on a development host, after the loopback check: a host that does not call it
/// provides none of the four ports.
///
/// # Errors
///
/// A port that another module already provides.
pub fn provide_runtimes(ctx: &mut SiteCtx<'_>, engine: ScriptEngine) -> Result<Runtimes> {
    let scripts = Arc::new(DevScripts::new(engine));
    let components = Arc::new(DevComponents::new());
    ctx.provide::<dyn ScriptRuntime>(Arc::clone(&scripts) as Arc<dyn ScriptRuntime>)?;
    ctx.provide::<dyn ComponentRuntime>(Arc::clone(&components) as Arc<dyn ComponentRuntime>)?;
    ctx.provide::<DevScripts>(Arc::clone(&scripts))?;
    ctx.provide::<DevComponents>(Arc::clone(&components))?;
    Ok(Runtimes {
        scripts,
        components,
    })
}
