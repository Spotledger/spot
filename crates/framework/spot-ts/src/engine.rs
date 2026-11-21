//! [`ScriptEngine`]: compiles scripts, caches their bytecode, and runs them one execution at a time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rquickjs::context::intrinsic;
use rquickjs::{Context, Module, Runtime, module::WriteOptions};
use serde_json::Value;

use crate::bridge::{HostBridge, HostSurface};
use crate::cache::{Cache, Compiled, MODULE_NAME, ScriptId, intern};
use crate::error::{Result, ScriptError};
use crate::exec::{Closed, execute};
use crate::limits::Limits;
use crate::sha::sha256_hex;
use crate::sourcemap::SourceMap;

/// How the engine is built.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// The most bytes the compile cache holds (source plus bytecode) before it evicts the least
    /// recently used script. Default 16 MiB.
    pub cache_bytes: usize,
    /// The host functions scripts may call: the closed allow-list. Default: none.
    pub surface: HostSurface,
    /// Whether compiled scripts keep their bytecode (the default). With `false` every script runs
    /// from source: slower to start, the same in every other respect. This is what a script gets
    /// anyway once the process-wide bytecode budget is spent.
    pub keep_bytecode: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            cache_bytes: 16 * 1024 * 1024,
            surface: HostSurface::default(),
            keep_bytecode: true,
        }
    }
}

/// What the engine has done so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineStats {
    /// Scripts compiled (cache misses).
    pub compiles: u64,
    /// Calls to `compile` answered from the cache.
    pub cache_hits: u64,
    /// Scripts in the cache now.
    pub cached_scripts: usize,
    /// Bytes the cache holds now (source plus bytecode).
    pub cached_bytes: usize,
}

struct Inner {
    cache: Mutex<Cache>,
    surface: HostSurface,
    keep_bytecode: bool,
    compiles: AtomicU64,
    hits: AtomicU64,
}

/// The engine: cheap to clone and share (it is a handle), holds the compile cache and the
/// allow-list. Every execution gets its own QuickJS runtime and context, made for it and dropped
/// with it; nothing is shared between executions.
#[derive(Clone)]
pub struct ScriptEngine {
    inner: Arc<Inner>,
}

type Intrinsics = (
    (
        intrinsic::Eval,
        intrinsic::Date,
        intrinsic::Json,
        intrinsic::RegExp,
    ),
    (
        intrinsic::RegExpCompiler,
        intrinsic::MapSet,
        intrinsic::TypedArrays,
        intrinsic::Promise,
    ),
);

impl ScriptEngine {
    /// An engine with this configuration.
    pub fn new(config: EngineConfig) -> ScriptEngine {
        ScriptEngine {
            inner: Arc::new(Inner {
                cache: Mutex::new(Cache::new(config.cache_bytes)),
                surface: config.surface,
                keep_bytecode: config.keep_bytecode,
                compiles: AtomicU64::new(0),
                hits: AtomicU64::new(0),
            }),
        }
    }

    /// Compiles `source_js`, an ES module whose exported functions are the script's entry points,
    /// to QuickJS bytecode, and caches it under `id`. An unchanged script (same id, same source) is
    /// returned from the cache without being compiled again; a changed version is a new entry.
    ///
    /// `source_map`, if given, is the version 3 map from the TypeScript the author wrote to
    /// `source_js`; it lets an uncaught exception name its original position.
    ///
    /// # Errors
    ///
    /// `script.compile` if the source does not compile or the map is malformed.
    pub fn compile(
        &self,
        source_js: &str,
        id: ScriptId,
        source_map: Option<&str>,
    ) -> Result<Arc<Compiled>> {
        let hash = sha256_hex(source_js.as_bytes());
        if let Ok(mut cache) = self.inner.cache.lock()
            && let Some(found) = cache.get(&id, &hash)
        {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(found);
        }
        let map = source_map
            .map(|text| {
                SourceMap::parse(text)
                    .map_err(|e| ScriptError::Compile(format!("the source map: {e}")))
            })
            .transpose()?;
        let bytecode = compile_to_bytecode(source_js)?;
        let bytecode_len = bytecode.len();
        let bundle = if self.inner.keep_bytecode {
            intern(bytecode)
        } else {
            None
        };
        let compiled = Arc::new(Compiled {
            id,
            source: source_js.into(),
            hash,
            bytecode_len: if bundle.is_some() { bytecode_len } else { 0 },
            bundle,
            map,
        });
        self.inner.compiles.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cache) = self.inner.cache.lock() {
            cache.put(Arc::clone(&compiled));
        }
        Ok(compiled)
    }

    /// Runs the exported function `entry` of `compiled`, passing it `args` (one JSON value) and
    /// returning its result as JSON (a promise is awaited). The execution runs on a thread of its
    /// own and never blocks a reactor thread.
    ///
    /// # Errors
    ///
    /// A [`ScriptError`]: a limit exceeded, a missing entry, an exception the script did not
    /// handle, or the script's own validation error.
    pub async fn run(
        &self,
        compiled: &Arc<Compiled>,
        entry: &str,
        args: &Value,
        bridge: Arc<dyn HostBridge>,
        limits: Limits,
    ) -> Result<Value> {
        let limits = clamp(limits);
        let engine = self.clone();
        let (compiled, entry, args) = (Arc::clone(compiled), entry.to_owned(), args.clone());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("spot-ts".into())
            .stack_size(stack_for(&limits))
            .spawn(move || {
                let result = execute(
                    &compiled,
                    &engine.inner.surface,
                    &entry,
                    &args,
                    bridge,
                    limits,
                );
                // The caller may have stopped waiting; the result is then dropped.
                let _ = sender.send(result);
            })
            .map_err(|e| {
                ScriptError::Internal(format!("cannot start the execution thread: {e}"))
            })?;
        receiver.await.map_err(|_| {
            ScriptError::Internal("the execution thread ended without a result".into())
        })?
    }

    /// [`ScriptEngine::run`] for a caller that is not async: runs the execution on a thread of its
    /// own and waits for it.
    pub fn run_blocking(
        &self,
        compiled: &Compiled,
        entry: &str,
        args: &Value,
        bridge: Arc<dyn HostBridge>,
        limits: Limits,
    ) -> Result<Value> {
        let limits = clamp(limits);
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("spot-ts".into())
                .stack_size(stack_for(&limits))
                .spawn_scoped(scope, || {
                    execute(compiled, &self.inner.surface, entry, args, bridge, limits)
                })
                .map_err(|e| {
                    ScriptError::Internal(format!("cannot start the execution thread: {e}"))
                })?
                .join()
                .map_err(|_| ScriptError::Internal("the execution thread panicked".into()))?
        })
    }

    /// What the engine has done so far.
    pub fn stats(&self) -> EngineStats {
        let (cached_scripts, cached_bytes) = self
            .inner
            .cache
            .lock()
            .map_or((0, 0), |c| (c.len(), c.bytes()));
        EngineStats {
            compiles: self.inner.compiles.load(Ordering::Relaxed),
            cache_hits: self.inner.hits.load(Ordering::Relaxed),
            cached_scripts,
            cached_bytes,
        }
    }

    /// Whether `id` is in the cache now.
    pub fn is_cached(&self, id: &ScriptId) -> bool {
        self.inner.cache.lock().is_ok_and(|c| c.contains(id))
    }
}

/// The most native stack a script may be given, whatever the limits ask for.
const MAX_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Room above the script's stack for the frames of this crate and of QuickJS' own bookkeeping.
const STACK_HEADROOM: usize = 1024 * 1024;

/// Limits with the stack allowance clamped to what a thread is created with.
fn clamp(limits: Limits) -> Limits {
    Limits {
        stack_bytes: limits.stack_bytes.clamp(16 * 1024, MAX_STACK_BYTES),
        ..limits
    }
}

/// The size of the thread that runs an execution. QuickJS checks its own `stack_bytes` allowance
/// against the real native stack, so the thread must have more room than the allowance: on an
/// ordinary thread (2 MiB) a larger allowance would let a script overflow the real stack, which
/// aborts the process. This crate therefore never runs a script on a thread it did not size.
fn stack_for(limits: &Limits) -> usize {
    limits.stack_bytes + STACK_HEADROOM
}

/// Compiles `source` as a module in a throwaway runtime and writes its bytecode. The runtime has
/// the same limits that protect an execution: a hostile source cannot exhaust the host while it is
/// being compiled.
fn compile_to_bytecode(source: &str) -> Result<Vec<u8>> {
    let internal = |e: rquickjs::Error| ScriptError::Internal(format!("compile: {e}"));
    let runtime = Runtime::new().map_err(internal)?;
    runtime.set_memory_limit(64 * 1024 * 1024);
    runtime.set_max_stack_size(512 * 1024);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    runtime.set_interrupt_handler(Some(Box::new(move || {
        std::time::Instant::now() >= deadline
    })));
    // Imports are refused here too, so a script that imports something fails to compile with the
    // reason, instead of with "could not load module".
    runtime.set_loader(Closed, Closed);
    let context = Context::custom::<Intrinsics>(&runtime).map_err(internal)?;
    context.with(|ctx| {
        let module =
            Module::declare(ctx.clone(), MODULE_NAME, source.as_bytes()).map_err(|error| {
                let message = match error {
                    rquickjs::Error::Exception => ctx
                        .catch()
                        .as_object()
                        .and_then(|o| {
                            o.get::<_, Option<rquickjs::Coerced<String>>>("message")
                                .ok()
                                .flatten()
                        })
                        .map_or_else(|| "the source does not compile".to_owned(), |m| m.0),
                    other => other.to_string(),
                };
                ScriptError::Compile(message)
            })?;
        module
            .write(WriteOptions {
                strip_source: true,
                ..WriteOptions::default()
            })
            .map_err(internal)
    })
}
