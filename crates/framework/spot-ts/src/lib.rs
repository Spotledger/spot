#![forbid(unsafe_code)]
//! The embedded JavaScript engine for TypeScript scripts (WP-EXT-08.T1, ADR-054).
//!
//! TypeScript is stripped to JavaScript on save (WP-EXT-08.T2); this crate runs that JavaScript on
//! QuickJS (through `rquickjs`), embedded in the binary. The acceptance sentence of the work
//! package is the design: *a runaway script is killed without affecting the runtime, and startup
//! and per-call latency stay within budget.*
//!
//! # Shape
//!
//! - [`ScriptEngine`]: cheap to share; holds the compile cache and the allow-list of host
//!   functions. [`ScriptEngine::compile`] turns an ES module into a [`Compiled`] (QuickJS bytecode
//!   and a hash of the source), cached by [`ScriptId`] (app, name, version) with a size cap and
//!   least-recently-used eviction. [`ScriptEngine::run`] calls an exported function of a compiled
//!   script as `entry(args)` on a blocking thread.
//! - **One runtime and context per execution**, made for it and dropped with it (`exec.rs`): no
//!   global, no memory and no handle is shared between calls or tenants.
//! - [`Limits`]: memory, native stack, a wall-clock deadline and an instruction budget (both
//!   enforced by the interrupt handler, which QuickJS makes **uncatchable**: a script that catches
//!   the interrupt and loops on is killed at the next tick), host calls and output size. Each
//!   violation is a typed [`ScriptError`] with a `script.*` code.
//! - [`HostBridge`]: the only way a script touches the world, behind a closed [`HostSurface`] with
//!   argument schemas, exposed to the script as a frozen `host` object. There is no `require`, no
//!   import of anything but the script itself, no `eval` or `Function` constructor, no `fetch`, no
//!   timers, no filesystem, network or process; `Date.now()`, `new Date()` and `Math.random()`
//!   answer from the invocation ([`HostBridge::now_millis`], [`HostBridge::random_seed`]).
//! - **Determinism**: the same script, arguments and bridge answers give the same result and the
//!   same host call sequence.
//! - [`ScriptError::into_app_error`]: the documented mapping to the app API's `AppError`.
//!
//! # Scripts are ES modules
//!
//! A script is a module whose exported functions are its entry points: `export function
//! on_validate(doc) { ... }`. QuickJS can write and read bytecode only for modules. An entry
//! receives one argument (the JSON given to `run`) and may be `async`.
//!
//! # Throwing typed errors
//!
//! A script signals a validation problem by throwing an object with a string `code`, and
//! optionally a `message` and `fields` (an array of `{ field, code, message }`):
//!
//! ```js
//! throw { code: "selling.qty_not_positive", message: "Quantity must be positive.",
//!         fields: [{ field: "items[2].qty", code: "selling.qty_not_positive", message: "..." }] };
//! ```
//!
//! That is [`ScriptError::Validation`]; [`ScriptError::into_app_error`] maps it to a
//! `Validation`-class `AppError` keeping the script's code and the field errors. Every other
//! failure maps to class `Internal` with its `script.*` code.
//!
//! # Bytecode is interned for the life of the process
//!
//! See [`cache`](crate::interned_bytes) and the module documentation for why and how it is bounded.

mod bridge;
mod cache;
mod engine;
mod error;
mod exec;
mod limits;
mod sha;
mod sourcemap;

pub use bridge::{HostBridge, HostFn, HostSurface, Shape};
pub use cache::{Compiled, INTERN_BUDGET, ScriptId, interned_bytes};
pub use engine::{EngineConfig, EngineStats, ScriptEngine};
pub use error::{FieldProblem, HostError, Result, ScriptError, SourcePosition};
pub use limits::Limits;
pub use sourcemap::SourceMap;
