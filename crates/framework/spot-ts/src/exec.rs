//! One execution: a QuickJS runtime and context made for it and dropped with it.
//!
//! Nothing here is shared between executions, tenants or calls: the runtime, its memory limit, its
//! interrupt handler, the `host` object and every global the script sets die together.
//!
//! # What a script can reach
//!
//! The global object has the standard built-ins this engine is given (see [`INTRINSICS`]) and one
//! more thing, `host`, a frozen object holding the allow-listed host functions plus `now()` and
//! `random()`. There is no `require`, no module resolution beyond the script itself (every
//! `import` fails), no `fetch`, no timers, no filesystem, network or process access: QuickJS
//! itself provides none of them and nothing here adds them. Three things the engine *does* have
//! are taken away before the script runs:
//!
//! - `eval` and the `Function` constructors (and the async and generator ones): QuickJS cannot
//!   compile code at all without the `Eval` intrinsic (the embedder needs it to compile the script
//!   itself), so they are replaced by functions that throw.
//! - `Date.now()` and `new Date()` with no argument: they answer with the invocation's time.
//! - `Math.random()`: it answers from the invocation's seed.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use rquickjs::context::intrinsic;
use rquickjs::loader::bundle::Bundle;
use rquickjs::loader::{Loader, Resolver};
use rquickjs::{Ctx, Exception, Function, Module, Object, Runtime, Value};
use serde_json::Value as Json;

use crate::bridge::{HostBridge, HostSurface};
use crate::cache::{Compiled, MODULE_NAME};
use crate::error::{FieldProblem, Result, ScriptError};
use crate::limits::Limits;

/// The built-ins a script gets, besides the always-present core of the language (`Object`,
/// `Array`, `String`, `Number`, `Math`, `Symbol`, errors, ...): `Date`, `JSON`, `RegExp`, `Map` and
/// `Set`, typed arrays, and promises. `Eval` is needed to compile anything (see the module docs).
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

/// Instructions between two calls of QuickJS's interrupt handler.
const TICK_INSTRUCTIONS: u64 = 10_000;

/// Why the engine stopped an execution, as the interrupt handler and the host functions record it.
const NONE: u8 = 0;
const TIMEOUT: u8 = 1;
const BUDGET: u8 = 2;
const HOST_CALLS: u8 = 3;
const HOST_PANIC: u8 = 4;

/// State the interrupt handler, the host functions and the caller all see.
struct Shared {
    reason: AtomicU8,
    ticks: AtomicU64,
    host_calls: AtomicU32,
    panicked: Mutex<String>,
    deadline: Instant,
    budget_ticks: u64,
}

impl Shared {
    /// The interrupt handler: stop (uncatchably) once a limit was hit.
    fn tick(&self) -> bool {
        if self.reason.load(Ordering::Acquire) != NONE {
            return true;
        }
        let ticks = self.ticks.fetch_add(1, Ordering::AcqRel) + 1;
        if Instant::now() >= self.deadline {
            self.reason.store(TIMEOUT, Ordering::Release);
            return true;
        }
        if ticks >= self.budget_ticks {
            self.reason.store(BUDGET, Ordering::Release);
            return true;
        }
        false
    }

    fn stop(&self, reason: u8) {
        // The first reason wins.
        let _ = self
            .reason
            .compare_exchange(NONE, reason, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// A resolver and loader that refuse everything: used when a script runs from source, where the
/// script itself is declared directly and every `import` must fail.
pub(crate) struct Closed;

impl Resolver for Closed {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        Err(rquickjs::Error::new_resolving_message(
            base,
            name,
            "imports are not allowed in scripts",
        ))
    }
}

impl Loader for Closed {
    fn load<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js>> {
        Err(rquickjs::Error::new_loading_message(
            name,
            "imports are not allowed in scripts",
        ))
    }
}

/// Runs `entry(args)` of `compiled` in a fresh runtime under `limits`.
pub(crate) fn execute(
    compiled: &Compiled,
    surface: &HostSurface,
    entry: &str,
    args: &Json,
    bridge: Arc<dyn HostBridge>,
    limits: Limits,
) -> Result<Json> {
    let internal = |what: &str, e: rquickjs::Error| ScriptError::Internal(format!("{what}: {e}"));
    let runtime = Runtime::new().map_err(|e| internal("cannot create the runtime", e))?;
    runtime.set_memory_limit(limits.memory_bytes);
    runtime.set_max_stack_size(limits.stack_bytes);
    let shared = Arc::new(Shared {
        reason: AtomicU8::new(NONE),
        ticks: AtomicU64::new(0),
        host_calls: AtomicU32::new(0),
        panicked: Mutex::new(String::new()),
        deadline: Instant::now() + limits.wall_clock,
        budget_ticks: (limits.instructions / TICK_INSTRUCTIONS).max(1),
    });
    let watcher = Arc::clone(&shared);
    runtime.set_interrupt_handler(Some(Box::new(move || watcher.tick())));
    match compiled.bundle {
        Some(bundle) => runtime.set_loader(Bundle(bundle), Bundle(bundle)),
        None => runtime.set_loader(Closed, Closed),
    }
    let context = rquickjs::Context::custom::<Intrinsics>(&runtime)
        .map_err(|e| internal("cannot create the context", e))?;
    let args_text =
        serde_json::to_string(args).map_err(|e| ScriptError::BadArguments(e.to_string()))?;

    let outcome = context.with(|ctx| {
        let result = run_in(
            &ctx, compiled, surface, entry, &args_text, &bridge, &shared, &limits,
        );
        result.map_err(|error| translate(&ctx, error, compiled))
    });

    // A limit the engine enforced decides the error, whatever the script saw or caught.
    let failure = match shared.reason.load(Ordering::Acquire) {
        TIMEOUT => Some(ScriptError::Timeout {
            millis: limits.wall_clock.as_millis(),
        }),
        BUDGET => Some(ScriptError::OutOfBudget {
            instructions: limits.instructions,
        }),
        HOST_CALLS => Some(ScriptError::TooManyHostCalls {
            limit: limits.host_calls,
        }),
        HOST_PANIC => Some(ScriptError::HostPanic(
            shared
                .panicked
                .lock()
                .map(|n| n.clone())
                .unwrap_or_default(),
        )),
        _ => None,
    };
    drop(context);
    drop(runtime);
    match (failure, outcome) {
        (Some(failure), _) => Err(failure),
        (None, Err(ScriptError::MemoryLimit { .. })) => Err(ScriptError::MemoryLimit {
            limit: limits.memory_bytes,
        }),
        (None, outcome) => {
            let text = outcome?;
            if text.len() > limits.output_bytes {
                return Err(ScriptError::OutputTooLarge {
                    limit: limits.output_bytes,
                });
            }
            serde_json::from_str(&text)
                .map_err(|e| ScriptError::Internal(format!("the result is not JSON: {e}")))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_in<'js>(
    ctx: &Ctx<'js>,
    compiled: &Compiled,
    surface: &HostSurface,
    entry: &str,
    args_text: &str,
    bridge: &Arc<dyn HostBridge>,
    shared: &Arc<Shared>,
    limits: &Limits,
) -> rquickjs::Result<String> {
    install_host(ctx, surface, bridge, shared, limits)?;
    let namespace: Object = match compiled.bundle {
        Some(_) => Module::import(ctx, MODULE_NAME)?.finish()?,
        None => {
            let (module, promise) =
                Module::declare(ctx.clone(), MODULE_NAME, compiled.source.as_bytes())?.eval()?;
            promise.finish::<()>()?;
            module.namespace()?
        }
    };
    let function = namespace
        .get::<_, Value>(entry)?
        .into_function()
        .ok_or_else(|| Exception::throw_message(ctx, &format!("{MISSING_ENTRY}{entry}")))?;
    let argument = ctx.json_parse(args_text)?;
    let mut returned: Value = function.call((argument,))?;
    if let Some(promise) = returned.as_promise().cloned() {
        returned = promise.finish::<Value>()?;
    }
    Ok(ctx
        .json_stringify(returned)?
        .map(|s| s.to_string())
        .transpose()?
        .unwrap_or_else(|| "null".to_owned()))
}

/// Marks the exception `run_in` throws for a missing entry point, so `translate` can tell it from
/// the script's own.
const MISSING_ENTRY: &str = "spot-ts:missing-entry:";

/// Builds the frozen `host` object and runs the prelude that closes the global object.
fn install_host<'js>(
    ctx: &Ctx<'js>,
    surface: &HostSurface,
    bridge: &Arc<dyn HostBridge>,
    shared: &Arc<Shared>,
    limits: &Limits,
) -> rquickjs::Result<()> {
    let host = Object::new(ctx.clone())?;
    for function in &surface.functions {
        let (name, schema) = (function.name.clone(), function.args_schema.clone());
        let (bridge, shared) = (Arc::clone(bridge), Arc::clone(shared));
        let max_calls = limits.host_calls;
        let call = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  argument: rquickjs::function::Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                if shared.host_calls.fetch_add(1, Ordering::AcqRel) >= max_calls {
                    shared.stop(HOST_CALLS);
                    return Err(host_error(
                        &ctx,
                        "limit",
                        "the host call limit was reached",
                    )?);
                }
                let text = match argument.0 {
                    Some(value) if !value.is_undefined() => ctx
                        .json_stringify(value)?
                        .map(|s| s.to_string())
                        .transpose()?
                        .unwrap_or_else(|| "null".to_owned()),
                    _ => "null".to_owned(),
                };
                let value: Json = serde_json::from_str(&text)
                    .map_err(|_| Exception::throw_type(&ctx, "the argument is not JSON"))?;
                if let Some(why) = schema.violation(&value, &name) {
                    return Err(host_error(&ctx, "bad_arguments", &why)?);
                }
                match catch_unwind(AssertUnwindSafe(|| bridge.call(&name, value))) {
                    Ok(Ok(result)) => ctx.json_parse(result.to_string()),
                    Ok(Err(error)) => Err(host_error(&ctx, &error.code, &error.message)?),
                    Err(_) => {
                        if let Ok(mut who) = shared.panicked.lock() {
                            who.clone_from(&name);
                        }
                        shared.stop(HOST_PANIC);
                        Err(host_error(&ctx, "panic", "the host function failed")?)
                    }
                }
            },
        )?;
        host.set(function.name.as_str(), call)?;
    }
    let clock = Arc::clone(bridge);
    host.set(
        "now",
        Function::new(ctx.clone(), move || clock.now_millis())?,
    )?;
    // Pseudo-random 53-bit integers from the invocation's seed (splitmix64); the prelude turns them
    // into numbers in [0, 1) so that no floating point arithmetic is written in this crate.
    let state = Rc::new(Cell::new(bridge.random_seed()));
    host.set(
        "__bits",
        Function::new(ctx.clone(), move || -> i64 {
            let z = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
            state.set(z);
            let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            i64::try_from((z ^ (z >> 31)) >> 11).unwrap_or(0)
        })?,
    )?;
    ctx.globals().set("host", host)?;
    ctx.eval::<(), _>(PRELUDE)
}

/// Creates the error a host function throws (`name` `HostError`, with a `code`).
fn host_error<'js>(ctx: &Ctx<'js>, code: &str, message: &str) -> rquickjs::Result<rquickjs::Error> {
    let exception = Exception::from_message(ctx.clone(), message)?;
    exception.as_object().set("name", "HostError")?;
    exception.as_object().set("code", code)?;
    Ok(exception.throw())
}

/// Runs once per execution, before the script, with the real `eval` still in place for the
/// embedder: closes what must not be open and freezes `host`.
const PRELUDE: &str = r#"
(() => {
  "use strict";
  const g = globalThis;
  const host = g.host;
  const now = host.now;
  const bits = host.__bits;
  delete host.__bits;
  const RealDate = g.Date;
  const Safe = class Date extends RealDate {
    constructor(...a) { if (a.length === 0) { super(now()); } else { super(...a); } }
    static now() { return now(); }
  };
  Object.defineProperty(g, "Date", { value: Safe, writable: false, configurable: false });
  host.random = function random() { return bits() / 9007199254740992; };
  Object.defineProperty(g.Math, "random", { value: host.random, writable: false, configurable: false });
  const refuse = function () { throw new TypeError("code generation from strings is disabled"); };
  Object.defineProperty(g, "eval", { value: refuse, writable: false, configurable: false });
  const holders = [
    Function.prototype,
    Object.getPrototypeOf(async function () {}),
    Object.getPrototypeOf(function* () {}),
    Object.getPrototypeOf(async function* () {}),
  ];
  for (const proto of holders) {
    Object.defineProperty(proto, "constructor", { value: refuse, writable: false, configurable: false });
  }
  Object.defineProperty(g, "Function", { value: refuse, writable: false, configurable: false });
  Object.freeze(host);
  Object.defineProperty(g, "host", { value: host, writable: false, configurable: false });
})();
"#;

/// Turns an engine error into a [`ScriptError`], reading the exception the script threw.
fn translate<'js>(ctx: &Ctx<'js>, error: rquickjs::Error, compiled: &Compiled) -> ScriptError {
    match error {
        rquickjs::Error::Exception => thrown(ctx, ctx.catch(), compiled),
        rquickjs::Error::Allocation => ScriptError::MemoryLimit { limit: 0 },
        rquickjs::Error::Resolving { .. } | rquickjs::Error::Loading { .. } => {
            ScriptError::Thrown {
                name: "ImportError".into(),
                message: "imports are not allowed in scripts".into(),
                stack: None,
                position: None,
            }
        }
        other => ScriptError::Internal(other.to_string()),
    }
}

fn thrown<'js>(ctx: &Ctx<'js>, value: Value<'js>, compiled: &Compiled) -> ScriptError {
    let describe = |v: &Value<'js>| -> String {
        ctx.json_stringify(v.clone())
            .ok()
            .flatten()
            .and_then(|s| s.to_string().ok())
            .or_else(|| v.as_string().and_then(|s| s.to_string().ok()))
            .unwrap_or_else(|| "(not printable)".to_owned())
    };
    // QuickJS reports memory exhaustion by throwing `null` when it cannot even build an error
    // object. (A script that throws `null` on purpose is reported the same way; that is rare enough
    // to accept, and a memory limit is the safe thing to call it.)
    if value.is_null() || value.is_undefined() {
        return ScriptError::MemoryLimit { limit: 0 };
    }
    let Some(object) = value.as_object() else {
        return ScriptError::Thrown {
            name: "Thrown".into(),
            message: describe(&value),
            stack: None,
            position: None,
        };
    };
    let text = |key: &str| {
        object
            .get::<_, Option<rquickjs::Coerced<String>>>(key)
            .ok()
            .flatten()
            .map(|c| c.0)
    };
    let name = text("name").unwrap_or_else(|| "Error".to_owned());
    let message = text("message").unwrap_or_default();
    if let Some(entry) = message.strip_prefix(MISSING_ENTRY) {
        return ScriptError::MissingEntry(entry.to_owned());
    }
    if message.contains("out of memory") {
        return ScriptError::MemoryLimit { limit: 0 };
    }
    if message.contains("stack overflow") || message.contains("call stack size exceeded") {
        return ScriptError::StackOverflow;
    }
    // A script's own typed validation error: an object with a string `code` (a host error also has
    // one, but it is the host's, and stays an ordinary throw).
    if name != "HostError"
        && let Some(code) = text("code")
    {
        let fields = object
            .get::<_, Option<rquickjs::Array>>("fields")
            .ok()
            .flatten()
            .map(|array| {
                array
                    .iter::<Object>()
                    .filter_map(|item| item.ok())
                    .filter_map(|item| {
                        let field = |k: &str| {
                            item.get::<_, Option<rquickjs::Coerced<String>>>(k)
                                .ok()
                                .flatten()
                                .map(|c| c.0)
                        };
                        Some(FieldProblem {
                            field: field("field")?,
                            code: field("code")?,
                            message: field("message").unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let message = if message.is_empty() {
            code.clone()
        } else {
            message
        };
        return ScriptError::Validation {
            code,
            message,
            fields,
        };
    }
    let stack = text("stack");
    let position = stack
        .as_deref()
        .and_then(first_position)
        .and_then(|(l, c)| compiled.map.as_ref()?.lookup(l, c));
    ScriptError::Thrown {
        name,
        message,
        stack,
        position,
    }
}

/// The line and column of the first frame of a QuickJS stack that is in the script itself
/// (`(script:LINE:COLUMN)`).
fn first_position(stack: &str) -> Option<(u32, u32)> {
    let marker = format!("{MODULE_NAME}:");
    for line in stack.lines() {
        if let Some(at) = line.find(&marker) {
            let rest = &line[at + marker.len()..];
            let mut parts = rest.trim_end_matches(')').split(':');
            let (l, c) = (parts.next()?, parts.next()?);
            if let (Ok(l), Ok(c)) = (l.trim().parse(), c.trim().parse()) {
                return Some((l, c));
            }
        }
    }
    None
}
