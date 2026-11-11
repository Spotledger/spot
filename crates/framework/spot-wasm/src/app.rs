//! [`AppInstance`]: a linked app, and the one way to call it.
//!
//! **An instance per call, from the pool, and nothing survives between calls.** `AppInstance` holds
//! the linked component (`InstancePre`); every [`AppInstance::call`] creates a fresh `Store` and a
//! fresh instance from the pool's slab, runs one export, and drops both. Linear memory, tables
//! and globals of one call are never seen by the next.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use wasmtime::component::{InstancePre, Linker, Val};
use wasmtime::{ResourceLimiter, Store, StoreContextMut, Trap};

use crate::capabilities::{GrantedCapabilities, IMPORTS};
use crate::error::{Result, WasmError};
use crate::host::{CompiledComponent, HostInner, WasmHost, first_line};
use crate::imports::{Imports, LogLevel};
use crate::json;
use crate::limits::Limits;

/// How often a long-running guest yields to the async runtime, in fuel units (about microseconds
/// of work), so one hostile loop cannot starve the other tasks on its thread.
const YIELD_EVERY_FUEL: u64 = 100_000;

/// Slack on top of the wall-clock limit before the async backstop gives up on a call that is stuck
/// somewhere the epoch cannot reach (a slow host import).
const BACKSTOP_GRACE: Duration = Duration::from_millis(250);

/// Why a call was stopped by the host rather than by the app.
#[derive(Debug)]
enum Violation {
    HostCalls,
    Capability(String),
    Memory(String),
}

/// Everything one call's store carries.
struct CallState {
    imports: Arc<dyn Imports>,
    capabilities: Arc<GrantedCapabilities>,
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
                "the app asked for {desired} bytes of memory; the limit is {}",
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
                "the app asked for {desired} table elements; the limit is {}",
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

/// A linked app. Cheap to clone.
#[derive(Clone)]
pub struct AppInstance {
    host: Arc<HostInner>,
    pre: InstancePre<CallState>,
    capabilities: Arc<GrantedCapabilities>,
    hash: String,
}

impl AppInstance {
    pub(crate) fn link(
        host: &WasmHost,
        component: &CompiledComponent,
        capabilities: GrantedCapabilities,
    ) -> Result<AppInstance> {
        // Denial first, with the import's name: the linker's own "unknown import" error would not
        // say which capability was missing.
        for import in component.imports() {
            if let Some(why) = capabilities.denial(import) {
                return Err(WasmError::CapabilityDenied(why));
            }
        }
        let capabilities = Arc::new(capabilities);
        let mut linker: Linker<CallState> = Linker::new(&host.inner.engine);
        provide(&mut linker, &capabilities)?;
        let pre = linker
            .instantiate_pre(&component.component)
            .map_err(|e| WasmError::BadComponent(first_line(&e)))?;
        Ok(AppInstance {
            host: Arc::clone(&host.inner),
            pre,
            capabilities,
            hash: component.content_hash().to_owned(),
        })
    }

    /// The content hash of the component this app runs.
    pub fn content_hash(&self) -> &str {
        &self.hash
    }

    /// The capabilities this app was linked with.
    pub fn capabilities(&self) -> &GrantedCapabilities {
        &self.capabilities
    }

    /// Calls the export `export` with `args` (a JSON array, one element per parameter; see
    /// [`crate::json`] conventions in the crate docs) under `limits`, in a fresh instance.
    ///
    /// The result is the export's single result as JSON (`null` for none, an array for several).
    /// If the export returns `result<T, E>`, `ok(T)` is the value and `err(E)` is
    /// [`WasmError::App`]: the app's own error, as JSON.
    ///
    /// The call waits for a pool slot if all are busy. However it ends, the slot is released and
    /// nothing of the call remains. `imports` serves the host side of the world for this call only.
    pub async fn call(
        &self,
        export: &str,
        args: &Value,
        limits: Limits,
        imports: Arc<dyn Imports>,
    ) -> Result<Value> {
        let permit = self
            .host
            .slots
            .acquire()
            .await
            .map_err(|_| WasmError::Internal("the host is shutting down".into()))?;
        let result = tokio::time::timeout(
            limits.wall_clock + BACKSTOP_GRACE,
            self.run(export, args, limits, imports),
        )
        .await;
        drop(permit);
        match result {
            Ok(result) => result,
            Err(_elapsed) => Err(WasmError::Timeout {
                millis: limits.wall_clock.as_millis(),
            }),
        }
    }

    async fn run(
        &self,
        export: &str,
        args: &Value,
        limits: Limits,
        imports: Arc<dyn Imports>,
    ) -> Result<Value> {
        let state = CallState {
            imports,
            capabilities: Arc::clone(&self.capabilities),
            host_calls: 0,
            host_call_limit: limits.host_calls,
            memory_limit: limits.memory_bytes.min(self.host.config.max_memory_bytes),
            table_limit: limits.table_elements,
            violation: None,
        };
        let mut store = Store::new(&self.host.engine, state);
        store.limiter(|state| state);
        let internal = |what: &str, e: wasmtime::Error| {
            WasmError::Internal(format!("{what}: {}", first_line(&e)))
        };
        store
            .set_fuel(limits.fuel)
            .map_err(|e| internal("cannot set fuel", e))?;
        store
            .fuel_async_yield_interval(Some(YIELD_EVERY_FUEL))
            .map_err(|e| internal("cannot set the yield interval", e))?;
        // One extra tick: the epoch may advance right after this line, so `n + 1` ticks guarantee
        // the call at least `n` full ticks.
        let tick = self.host.config.tick.as_nanos().max(1);
        let ticks = limits
            .wall_clock
            .as_nanos()
            .div_ceil(tick)
            .saturating_add(1);
        store.set_epoch_deadline(u64::try_from(ticks).unwrap_or(u64::MAX));
        store.epoch_deadline_trap();

        match self.invoke(&mut store, export, args, &limits).await {
            Ok(value) => Ok(value),
            Err(Failure::Wasm(error)) => Err(classify(&error, store.data(), &limits)),
            Err(Failure::Ours(error)) => Err(error),
        }
    }

    async fn invoke(
        &self,
        store: &mut Store<CallState>,
        export: &str,
        args: &Value,
        limits: &Limits,
    ) -> std::result::Result<Value, Failure> {
        let instance = self
            .pre
            .instantiate_async(&mut *store)
            .await
            .map_err(Failure::Wasm)?;
        let func = instance
            .get_func(&mut *store, export)
            .ok_or_else(|| Failure::Ours(WasmError::MissingExport(export.to_owned())))?;
        let ty = func.ty(&*store);
        let params: Vec<_> = ty.params().collect();
        let arguments = args.as_array().ok_or_else(|| {
            Failure::Ours(WasmError::BadArguments(
                "the arguments must be a JSON array".into(),
            ))
        })?;
        if arguments.len() != params.len() {
            return Err(Failure::Ours(WasmError::BadArguments(format!(
                "`{export}` takes {} argument(s), found {}",
                params.len(),
                arguments.len()
            ))));
        }
        let values = arguments
            .iter()
            .zip(&params)
            .map(|(value, (name, ty))| json::to_val(value, ty, name))
            .collect::<Result<Vec<Val>>>()
            .map_err(Failure::Ours)?;
        let mut results = vec![Val::Bool(false); ty.results().len()];
        func.call_async(&mut *store, &values, &mut results)
            .await
            .map_err(Failure::Wasm)?;
        output(&results, limits.output_bytes).map_err(Failure::Ours)
    }
}

/// Why `invoke` stopped: the runtime reported an error (to be classified), or we already know.
enum Failure {
    Wasm(wasmtime::Error),
    Ours(WasmError),
}

/// Turns the call's results into JSON, within the output limit.
fn output(results: &[Val], limit: usize) -> Result<Value> {
    let size: usize = results.iter().map(json::size_hint).sum();
    if size > limit {
        return Err(WasmError::OutputTooLarge { limit });
    }
    let unwrap = |val: &Val| -> Result<Value> {
        match val {
            Val::Result(Ok(payload)) => payload
                .as_ref()
                .map_or(Ok(Value::Null), |p| json::to_json(p)),
            Val::Result(Err(payload)) => Err(WasmError::App(
                payload
                    .as_ref()
                    .map_or(Ok(Value::Null), |p| json::to_json(p))?,
            )),
            other => json::to_json(other),
        }
    };
    let value = match results {
        [] => Value::Null,
        [one] => unwrap(one)?,
        many => Value::Array(many.iter().map(json::to_json).collect::<Result<_>>()?),
    };
    if value.to_string().len() > limit {
        return Err(WasmError::OutputTooLarge { limit });
    }
    Ok(value)
}

/// Says why a call failed: what the host recorded first, then what the runtime reports.
fn classify(error: &wasmtime::Error, state: &CallState, limits: &Limits) -> WasmError {
    match &state.violation {
        Some(Violation::HostCalls) => {
            return WasmError::TooManyHostCalls {
                limit: limits.host_calls,
            };
        }
        Some(Violation::Capability(why)) => return WasmError::CapabilityDenied(why.clone()),
        Some(Violation::Memory(why)) => return WasmError::Memory(why.clone()),
        None => {}
    }
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => WasmError::OutOfFuel { fuel: limits.fuel },
        Some(Trap::Interrupt) => WasmError::Timeout {
            millis: limits.wall_clock.as_millis(),
        },
        Some(Trap::StackOverflow) => WasmError::Trap("the app's call stack overflowed".into()),
        Some(trap) => WasmError::Trap(trap.to_string()),
        None => WasmError::Trap(first_line(error)),
    }
}

/// Defines, in `linker`, the host side of every import this app was granted, and no other.
fn provide(linker: &mut Linker<CallState>, capabilities: &GrantedCapabilities) -> Result<()> {
    let link = |e: wasmtime::Error| {
        WasmError::Internal(format!("cannot link a host function: {}", first_line(&e)))
    };
    let mut root = linker.root();
    for (name, _) in IMPORTS {
        if capabilities.denial(name).is_some() {
            continue;
        }
        match *name {
            "log" => root
                .func_wrap(
                    "log",
                    |mut cx: StoreContextMut<'_, CallState>,
                     (level, message): (LogLevel, String)| {
                        cx.data_mut().host_call()?;
                        cx.data().imports.log(level, &message);
                        Ok(())
                    },
                )
                .map_err(link)?,
            "now" => root
                .func_wrap("now", |mut cx: StoreContextMut<'_, CallState>, (): ()| {
                    cx.data_mut().host_call()?;
                    Ok((cx.data().imports.now(),))
                })
                .map_err(link)?,
            "get-doc" => root
                .func_wrap(
                    "get-doc",
                    |mut cx: StoreContextMut<'_, CallState>, (doctype, id): (String, String)| {
                        cx.data_mut().host_call()?;
                        if !cx.data().capabilities.may_read(&doctype) {
                            let why = format!("the app may not read `{doctype}` documents");
                            cx.data_mut()
                                .violation
                                .get_or_insert(Violation::Capability(why));
                            return Err(wasmtime::Error::msg("capability denied"));
                        }
                        Ok((cx.data().imports.get_doc(&doctype, &id),))
                    },
                )
                .map_err(link)?,
            _ => {}
        }
    }
    Ok(())
}
