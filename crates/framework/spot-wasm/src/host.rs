//! [`WasmHost`]: the engine, its pool and its clock.
//!
//! One host owns one `wasmtime::Engine`, configured once:
//!
//! - the component model, **fuel** and **epoch interruption** are on; threads are off (the cargo
//!   feature is not enabled) and relaxed SIMD is off (it is nondeterministic across machines);
//! - the **pooling allocator** serves every instance: all memories, tables, stacks and component
//!   instances come from slabs reserved up front, with hard caps, so a hostile app can neither
//!   exhaust the machine nor slow down instantiation for others;
//! - linear memories use the static layout with guard pages (the default on 64-bit hosts), so
//!   there are no bounds checks to skip and none to get wrong;
//! - **one ticker thread for the whole host** advances the engine's epoch every
//!   [`HostConfig::tick`]; a call's wall-clock limit is a number of ticks, so a call costs no
//!   timer and no thread of its own.
//!
//! A semaphore with one permit per pool slot sits in front of the pool: a call waits for a slot
//! instead of failing when all are busy, and the slot is back the moment the call ends, however
//! it ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::Semaphore;
use wasmtime::component::Component;
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

use crate::app::AppInstance;
use crate::capabilities::GrantedCapabilities;
use crate::error::{Result, WasmError};
use crate::hash::sha256_hex;

/// How the host is built. The defaults suit a server running many small app calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostConfig {
    /// Calls that may run at once; also the size of the pool (instances, stacks, memories). More
    /// calls wait for a slot. Default 64.
    pub max_concurrent_calls: u32,
    /// The largest linear memory any call can ever have: the pool's slab size per memory, and the
    /// ceiling for [`Limits::memory_bytes`](crate::Limits::memory_bytes). Default 32 MiB. Address
    /// space for `max_concurrent_calls * 2` of them is reserved up front (virtual, not resident).
    pub max_memory_bytes: usize,
    /// The granularity of wall-clock limits: the epoch advances this often. A call is stopped
    /// within one tick of its limit. Default 5 ms.
    pub tick: Duration,
    /// The wasm call stack, in bytes; deeper recursion traps. Default 512 KiB.
    pub stack_bytes: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            max_concurrent_calls: 64,
            max_memory_bytes: 32 * 1024 * 1024,
            tick: Duration::from_millis(5),
            stack_bytes: 512 * 1024,
        }
    }
}

/// Advances the engine's epoch until stopped.
struct Ticker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Ticker {
    fn start(engine: Engine, tick: Duration) -> Result<Ticker> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("spot-wasm-epoch".into())
            .spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    std::thread::park_timeout(tick);
                    if flag.load(Ordering::Acquire) {
                        break;
                    }
                    engine.increment_epoch();
                }
            })
            .map_err(|e| WasmError::Internal(format!("cannot start the epoch ticker: {e}")))?;
        Ok(Ticker {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

/// What every [`AppInstance`] of a host shares.
pub(crate) struct HostInner {
    pub(crate) engine: Engine,
    pub(crate) config: HostConfig,
    pub(crate) slots: Semaphore,
    // Dropped (and joined) with the host's last reference.
    _ticker: Ticker,
}

/// The host: the engine, the pool and the clock. Cheap to clone (it is a handle).
#[derive(Clone)]
pub struct WasmHost {
    pub(crate) inner: Arc<HostInner>,
}

/// A validated, compiled component with a content hash. Cheap to clone.
#[derive(Clone)]
pub struct CompiledComponent {
    pub(crate) component: Component,
    hash: String,
    imports: Vec<String>,
    exports: Vec<String>,
}

impl CompiledComponent {
    /// The SHA-256 of the component's bytes, as 64 lowercase hex digits: the key for a compile
    /// cache and for "this is the app that was installed".
    pub fn content_hash(&self) -> &str {
        &self.hash
    }

    /// The names of what the component imports, sorted.
    pub fn imports(&self) -> &[String] {
        &self.imports
    }

    /// The names of what the component exports, sorted.
    pub fn exports(&self) -> &[String] {
        &self.exports
    }
}

impl WasmHost {
    /// Builds the engine and starts the clock.
    pub fn new(config: HostConfig) -> Result<WasmHost> {
        let slots = config.max_concurrent_calls.max(1);
        let mut pool = PoolingAllocationConfig::new();
        pool.total_component_instances(slots)
            .total_core_instances(slots.saturating_mul(4))
            .max_core_instances_per_component(4)
            .total_memories(slots.saturating_mul(2))
            .max_memories_per_component(2)
            .max_memory_size(config.max_memory_bytes)
            .total_tables(slots.saturating_mul(2))
            .max_tables_per_component(2)
            .table_elements(100_000)
            .total_stacks(slots);

        let mut wasm = Config::new();
        wasm.wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true)
            .wasm_relaxed_simd(false)
            .wasm_backtrace_max_frames(None)
            .max_wasm_stack(config.stack_bytes)
            // The fiber stack must hold the wasm stack and the host frames above it.
            .async_stack_size(config.stack_bytes + 256 * 1024)
            .allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        let engine = Engine::new(&wasm)
            .map_err(|e| WasmError::Internal(format!("cannot build the engine: {e}")))?;
        let ticker = Ticker::start(engine.clone(), config.tick)?;
        Ok(WasmHost {
            inner: Arc::new(HostInner {
                engine,
                config,
                slots: Semaphore::new(slots as usize),
                _ticker: ticker,
            }),
        })
    }

    /// Validates and compiles `bytes` (a component in binary form). `wasm.bad_component` if they are
    /// not one.
    pub fn compile(&self, bytes: &[u8]) -> Result<CompiledComponent> {
        let component = Component::new(&self.inner.engine, bytes)
            .map_err(|e| WasmError::BadComponent(first_line(&e)))?;
        let ty = component.component_type();
        let mut imports: Vec<String> = ty
            .imports(&self.inner.engine)
            .map(|(name, _)| name.to_owned())
            .collect();
        let mut exports: Vec<String> = ty
            .exports(&self.inner.engine)
            .map(|(name, _)| name.to_owned())
            .collect();
        imports.sort();
        exports.sort();
        Ok(CompiledComponent {
            component,
            hash: sha256_hex(bytes),
            imports,
            exports,
        })
    }

    /// Links `component` for an app that was granted `capabilities`. **Only the imports the
    /// capabilities allow are provided**: an app that imports anything else (or a gated import it
    /// was not granted) fails here with `wasm.capability_denied`, naming the import, before it
    /// ever runs.
    pub fn instantiate(
        &self,
        component: &CompiledComponent,
        capabilities: GrantedCapabilities,
    ) -> Result<AppInstance> {
        AppInstance::link(self, component, capabilities)
    }

    /// How many calls may still start without waiting. Back to [`WasmHost::slots`] when idle.
    pub fn available_slots(&self) -> usize {
        self.inner.slots.available_permits()
    }

    /// The pool size: how many calls may run at once.
    pub fn slots(&self) -> usize {
        self.inner.config.max_concurrent_calls.max(1) as usize
    }

    /// The configuration the host was built with.
    pub fn config(&self) -> HostConfig {
        self.inner.config
    }
}

/// The first line of an error's message: the part that says what is wrong, without the chain of
/// context and the backtrace.
pub(crate) fn first_line(error: &wasmtime::Error) -> String {
    let text = format!("{error:#}");
    text.lines().next().unwrap_or("unknown error").to_owned()
}
