//! The runtime that runs one connector call: a `wasmtime` component engine configured exactly
//! like `spot-wasm`'s (component model, fuel, epoch interruption, a pooling allocator with hard
//! caps), a linker that provides only the connector world's imports, and one fresh store and
//! instance per call.
//!
//! `spot-wasm::WasmHost` cannot be reused directly: its import table is fixed to the app test
//! world (`WP-EXT-05.T1`) and it has no way to link another world. The engine configuration,
//! the limits vocabulary ([`spot_wasm::Limits`]), the tick ([`spot_wasm::HostConfig`]) and the
//! typed errors ([`spot_wasm::WasmError`]) are reused; only the linker is the connector's own.
//! When `WP-EXT-05.T2` makes the app world's import table extensible, this should move onto
//! `WasmHost`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::Semaphore;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, ResourceLimiter, Store,
    Trap,
};

use crate::bindings::{self, Connector, ConnectorImports};
use crate::error::{ConnectorError, Result};
use crate::imports::CallServices;
use crate::model::{AuthKind, CallContext, ConnectorInfo, Operation};
use spot_wasm::{HostConfig, Limits, LogLevel};

/// How often a long-running guest yields to the async runtime, in fuel units.
const YIELD_EVERY_FUEL: u64 = 100_000;

/// Slack on top of the wall-clock limit before the async backstop gives up.
const BACKSTOP_GRACE: Duration = Duration::from_millis(250);

/// Why a call was stopped by the host rather than by the connector.
#[derive(Debug)]
enum Violation {
    HostCalls,
    Memory(String),
}

/// Everything one call's store carries.
struct CallState {
    services: Arc<CallServices>,
    host_calls: u32,
    host_call_limit: u32,
    memory_limit: usize,
    table_limit: usize,
    violation: Option<Violation>,
}

impl CallState {
    /// Counts one host call; fails (and records why) past the limit.
    fn host_call(&mut self) -> wasmtime::Result<()> {
        self.host_calls = self.host_calls.saturating_add(1);
        if self.host_calls > self.host_call_limit {
            self.violation.get_or_insert(Violation::HostCalls);
            return Err(wasmtime::Error::msg("host call limit exceeded"));
        }
        Ok(())
    }
}

impl ResourceLimiter for CallState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_limit {
            let why = format!(
                "the connector asked for {desired} bytes of memory; the limit is {}",
                self.memory_limit
            );
            self.violation.get_or_insert(Violation::Memory(why));
            return Err(wasmtime::Error::msg("memory limit exceeded"));
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.table_limit {
            let why = format!(
                "the connector asked for {desired} table elements; the limit is {}",
                self.table_limit
            );
            self.violation.get_or_insert(Violation::Memory(why));
            return Err(wasmtime::Error::msg("table limit exceeded"));
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        8
    }

    fn tables(&self) -> usize {
        8
    }

    fn memories(&self) -> usize {
        4
    }
}

impl ConnectorImports for CallState {
    async fn log(&mut self, level: bindings::Level, message: String) -> wasmtime::Result<()> {
        self.host_call()?;
        self.services.logs.log(to_log_level(level), &message);
        self.services
            .monitor
            .record_host_call(&self.services.config.id);
        Ok(())
    }

    async fn http_request(
        &mut self,
        request: String,
    ) -> wasmtime::Result<std::result::Result<String, String>> {
        self.host_call()?;
        self.services
            .monitor
            .record_host_call(&self.services.config.id);
        Ok(self
            .services
            .http_request(&request)
            .await
            .map_err(|e| e.to_string()))
    }

    async fn secrets_get(
        &mut self,
        reference: String,
    ) -> wasmtime::Result<std::result::Result<String, String>> {
        self.host_call()?;
        Ok(self
            .services
            .secrets_get(&reference)
            .await
            .map_err(|e| e.to_string()))
    }

    async fn kv_get(&mut self, key: String) -> wasmtime::Result<Option<String>> {
        self.host_call()?;
        Ok(self.services.kv_get(&key))
    }

    async fn kv_set(
        &mut self,
        key: String,
        value: String,
    ) -> wasmtime::Result<std::result::Result<(), String>> {
        self.host_call()?;
        Ok(self
            .services
            .kv_set(&key, &value)
            .map_err(|e| e.to_string()))
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
            .name("spot-connectors-epoch".into())
            .spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    std::thread::park_timeout(tick);
                    if flag.load(Ordering::Acquire) {
                        break;
                    }
                    engine.increment_epoch();
                }
            })
            .map_err(|e| ConnectorError::HostFailure {
                reason: format!("cannot start the epoch ticker: {e}"),
            })?;
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

/// The connector runtime: one engine, one linker, one pool, one clock.
pub struct ConnectorRuntime {
    engine: Engine,
    linker: Linker<CallState>,
    config: HostConfig,
    slots: Semaphore,
    _ticker: Ticker,
}

impl ConnectorRuntime {
    /// Builds the engine and starts the clock. `config` is the same [`HostConfig`] an app host
    /// uses, so a connector runs under the same wall-clock tick, memory ceiling and pool.
    pub fn new(config: HostConfig) -> Result<Self> {
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
            .async_stack_size(config.stack_bytes + 256 * 1024)
            .allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        let engine = Engine::new(&wasm).map_err(|e| ConnectorError::HostFailure {
            reason: format!("cannot build the engine: {e}"),
        })?;
        let ticker = Ticker::start(engine.clone(), config.tick)?;
        let mut linker: Linker<CallState> = Linker::new(&engine);
        Connector::add_to_linker::<CallState, HasSelf<CallState>>(&mut linker, |state| state)
            .map_err(|e| ConnectorError::HostFailure {
                reason: format!("cannot link the connector world: {e}"),
            })?;
        Ok(Self {
            engine,
            linker,
            config,
            slots: Semaphore::new(slots as usize),
            _ticker: ticker,
        })
    }

    /// Validates and compiles `bytes` (a connector component in binary form).
    pub fn compile(&self, bytes: &[u8]) -> Result<Component> {
        Component::new(&self.engine, bytes).map_err(|e| ConnectorError::BadBundle {
            reason: first_line(&e),
        })
    }

    /// The configuration the runtime was built with.
    pub fn config(&self) -> HostConfig {
        self.config
    }

    async fn slot(&self) -> Result<tokio::sync::SemaphorePermit<'_>> {
        self.slots
            .acquire()
            .await
            .map_err(|_| ConnectorError::HostFailure {
                reason: "the runtime is shutting down".into(),
            })
    }

    fn store(&self, services: Arc<CallServices>) -> Store<CallState> {
        let limits = services.config.limits;
        let state = CallState {
            services,
            host_calls: 0,
            host_call_limit: limits.host_calls,
            memory_limit: limits.memory_bytes.min(self.config.max_memory_bytes),
            table_limit: limits.table_elements,
            violation: None,
        };
        Store::new(&self.engine, state)
    }

    /// Runs `describe` on a fresh instance.
    pub async fn describe(
        &self,
        component: &Component,
        services: Arc<CallServices>,
    ) -> Result<ConnectorInfo> {
        let permit = self.slot().await?;
        let mut store = self.store(Arc::clone(&services));
        let limits = services.config.limits;
        prepare(&mut store, &limits, self.config.tick)?;
        let result = with_deadline(limits.wall_clock, async {
            let instance = Connector::instantiate_async(&mut store, component, &self.linker)
                .await
                .map_err(|e| classify(&e, &store, &limits))?;
            let info = instance
                .call_describe(&mut store)
                .await
                .map_err(|e| classify(&e, &store, &limits))?;
            Ok(to_info(info))
        })
        .await;
        drop(permit);
        result
    }

    /// Runs `invoke` on a fresh instance, returning the JSON output text.
    pub async fn invoke(
        &self,
        component: &Component,
        services: Arc<CallServices>,
        operation: &str,
        input: &str,
        ctx: CallContext,
    ) -> Result<String> {
        let permit = self.slot().await?;
        let mut store = self.store(Arc::clone(&services));
        let limits = services.config.limits;
        prepare(&mut store, &limits, self.config.tick)?;
        let result = with_deadline(limits.wall_clock, async {
            let instance = Connector::instantiate_async(&mut store, component, &self.linker)
                .await
                .map_err(|e| classify(&e, &store, &limits))?;
            let ctx = to_wit_ctx(&ctx);
            let outcome = instance
                .call_invoke(&mut store, operation, input, &ctx)
                .await
                .map_err(|e| classify(&e, &store, &limits))?;
            let text = outcome.map_err(from_wit_error)?;
            check_output(&text, limits.output_bytes)?;
            Ok(text)
        })
        .await;
        drop(permit);
        result
    }

    /// Runs `webhook` on a fresh instance, returning the JSON response text.
    pub async fn webhook(
        &self,
        component: &Component,
        services: Arc<CallServices>,
        request: &str,
    ) -> Result<String> {
        let permit = self.slot().await?;
        let mut store = self.store(Arc::clone(&services));
        let limits = services.config.limits;
        prepare(&mut store, &limits, self.config.tick)?;
        let result = with_deadline(limits.wall_clock, async {
            let instance = Connector::instantiate_async(&mut store, component, &self.linker)
                .await
                .map_err(|e| classify(&e, &store, &limits))?;
            let outcome = instance
                .call_webhook(&mut store, request)
                .await
                .map_err(|e| classify(&e, &store, &limits))?;
            let text = outcome.map_err(from_wit_error)?;
            check_output(&text, limits.output_bytes)?;
            Ok(text)
        })
        .await;
        drop(permit);
        result
    }
}

fn prepare(store: &mut Store<CallState>, limits: &Limits, tick: Duration) -> Result<()> {
    let internal = |what: &str, e: wasmtime::Error| ConnectorError::HostFailure {
        reason: format!("{what}: {}", first_line(&e)),
    };
    store
        .set_fuel(limits.fuel)
        .map_err(|e| internal("cannot set fuel", e))?;
    store
        .fuel_async_yield_interval(Some(YIELD_EVERY_FUEL))
        .map_err(|e| internal("cannot set the yield interval", e))?;
    let tick = tick.as_nanos().max(1);
    let ticks = limits
        .wall_clock
        .as_nanos()
        .div_ceil(tick)
        .saturating_add(1);
    store.set_epoch_deadline(u64::try_from(ticks).unwrap_or(u64::MAX));
    store.epoch_deadline_trap();
    store.limiter(|state| state);
    Ok(())
}

async fn with_deadline<T>(
    wall_clock: Duration,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(wall_clock + BACKSTOP_GRACE, future).await {
        Ok(result) => result,
        Err(_elapsed) => Err(ConnectorError::Timeout {
            millis: wall_clock.as_millis(),
        }),
    }
}

fn check_output(text: &str, limit: usize) -> Result<()> {
    if text.len() > limit {
        return Err(ConnectorError::OutputTooLarge { limit });
    }
    Ok(())
}

fn classify(error: &wasmtime::Error, store: &Store<CallState>, limits: &Limits) -> ConnectorError {
    match &store.data().violation {
        Some(Violation::HostCalls) => {
            return ConnectorError::TooManyHostCalls {
                limit: limits.host_calls,
            };
        }
        Some(Violation::Memory(why)) => {
            return ConnectorError::Trapped {
                reason: why.clone(),
            };
        }
        None => {}
    }
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => ConnectorError::OutOfFuel { fuel: limits.fuel },
        Some(Trap::Interrupt) => ConnectorError::Timeout {
            millis: limits.wall_clock.as_millis(),
        },
        Some(Trap::StackOverflow) => ConnectorError::Trapped {
            reason: "the connector's call stack overflowed".into(),
        },
        Some(trap) => ConnectorError::Trapped {
            reason: trap.to_string(),
        },
        None => ConnectorError::Trapped {
            reason: first_line(error),
        },
    }
}

fn to_log_level(level: bindings::Level) -> LogLevel {
    match level {
        bindings::Level::Debug => LogLevel::Debug,
        bindings::Level::Info => LogLevel::Info,
        bindings::Level::Warn => LogLevel::Warn,
        bindings::Level::Error => LogLevel::Error,
    }
}

fn to_info(info: bindings::ConnectorInfo) -> ConnectorInfo {
    ConnectorInfo {
        name: info.name,
        version: info.version,
        operations: info
            .operations
            .into_iter()
            .map(|op| Operation {
                name: op.name,
                input_schema: op.input_schema,
                output_schema: op.output_schema,
            })
            .collect(),
        auth: info.auth.into_iter().map(to_auth_kind).collect(),
    }
}

fn to_auth_kind(kind: bindings::AuthKind) -> AuthKind {
    match kind {
        bindings::AuthKind::None => AuthKind::None,
        bindings::AuthKind::Bearer => AuthKind::Bearer,
        bindings::AuthKind::Basic => AuthKind::Basic,
        bindings::AuthKind::Oauth2 => AuthKind::OAuth2,
    }
}

fn to_wit_ctx(ctx: &CallContext) -> bindings::CallContext {
    bindings::CallContext {
        tenant: ctx.tenant.clone(),
        actor: ctx.actor.clone(),
        now_micros: ctx.now_micros,
        deadline_micros: ctx.deadline_micros,
    }
}

fn from_wit_error(error: bindings::Error) -> ConnectorError {
    ConnectorError::Guest {
        code: error.code,
        message: error.message,
        retryable: error.retryable,
    }
}

/// The first line of an error's message, without the context chain.
fn first_line(error: &wasmtime::Error) -> String {
    let text = format!("{error:#}");
    text.lines().next().unwrap_or("unknown error").to_owned()
}
