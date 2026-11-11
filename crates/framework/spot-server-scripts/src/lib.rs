#![forbid(unsafe_code)]
//! Document-event Server Scripts (WP-EXT-06.T1; ADR-054, ADR-51).
//!
//! A site administrator or consultant writes a short TypeScript function that runs when a
//! document is validated, saved, submitted or cancelled, without building an app. This crate
//! turns that text into a normal step of the document lifecycle: the [`ScriptController`]
//! implements the app API's `Controller` trait, so the document engine runs a script exactly
//! like any native or WASM step, on the embedded TypeScript engine ([`spot_ts`], ADR-054).
//!
//! # The shape
//!
//! - [`ServerScript`] is the record: its DocType, event, TypeScript source, timeout and
//!   priority, read from a `Server Script` document.
//! - [`ScriptEvent`] maps the nine document events (`before_validate`, `validate`, `before_save`,
//!   `after_save`, `before_submit`, `on_submit`, `before_cancel`, `on_cancel`, `after_commit`)
//!   onto ADR-51 hook points, and names the exported entry point (`on_validate`, ...).
//! - [`ScriptStore`] loads the enabled scripts for a DocType and records a compile result. A
//!   [`FakeScriptStore`] serves tests and [`DocumentScriptStore`] is the storage-backed one.
//! - **Compile on save**: [`ServerScriptController`] is the `server_script` DocType's own
//!   controller. Its `before_save` step transpiles the source with [`spot_ts_transpile`],
//!   reports every diagnostic with its line and column as a field error on the source field,
//!   refuses to save a script that does not compile, and stores the compiled hash.
//! - [`ScriptController`] is one controller per DocType: a step per (script, event), ordered by
//!   the numeric `priority` and then the script name, each running `on_<event>({ doc, ctx })`.
//!   (The engine's `ScriptEngine::run` passes a single JSON argument, so the entry receives one
//!   object with the document view under `doc` and the event context under `ctx`.)
//! - The **host surface** ([`host`]): a closed, schema-checked set of functions
//!   (`host.log`, `host.get`, `host.query`, `host.insert`, `host.update`, `host.call`,
//!   `host.throw`, `host.format_money`, `host.round`, and the engine's `host.now`). There is no
//!   network, no filesystem, no environment and no `fetch`.
//! - [`ScriptLimits`], [`ScriptLog`] and [`ScriptRun`]: per-script limits and observability.
//!
//! # Running as the triggering user
//!
//! A script never gets more rights than the user who triggered the operation. The document view
//! it is given is built through the host's [`FieldView`] (the same field access the API uses),
//! and every `host.get`/`host.query`/`host.insert`/`host.update` goes through the step's
//! [`HookContext`](spot_app_api::HookContext), whose own authorizer decides whether the acting
//! user may read the field, run the query or write the document. Elevation is a later, audited,
//! per-script flag and is not part of this task.
//!
//! # Determinism and limits
//!
//! The engine gives every execution its own runtime; this crate adds a per-script wall-clock
//! deadline (default 200 ms, site maximum 2 s), a memory, instruction, host-call and output
//! budget, and a deterministic clock and random seed taken from the invocation. A script that
//! exceeds any limit is killed and the operation fails with `serverscript.limit_exceeded`; in
//! `after_commit` the failure is logged and the script is disabled after a run of consecutive
//! failures.

mod codes;
mod compile;
mod controller;
mod error;
mod host;
mod limits;
mod log;
mod record;
mod store;
mod view;

pub use codes::{COMPILE_FAILED, LIMIT_EXCEEDED, PHASE_VIOLATION, RECORD_INVALID, STORE_FAILED};
pub use compile::{CompileOutcome, ServerScriptController, compile_source};
pub use controller::ScriptController;
pub use error::map_script_error;
pub use host::{
    HostRequest, MAX_QUERY_LIMIT, MethodCaller, ScriptBridge, TypedThrow, host_surface,
    typed_throw_error,
};
pub use limits::ScriptLimits;
pub use log::{LogLevel, ScriptLog, ScriptRun, ScriptTelemetry};
pub use record::{
    FIELD_COMPILED_HASH, FIELD_DISABLED_REASON, FIELD_DOCTYPE, FIELD_ENABLED, FIELD_EVENT,
    FIELD_LAST_ERROR, FIELD_LAST_RUN_COUNT, FIELD_NAME, FIELD_PRIORITY, FIELD_SCRIPT,
    FIELD_TIMEOUT_MS, MAX_PRIORITY, ScriptEvent, ServerScript,
};
pub use store::{DocumentScriptStore, FakeScriptStore, ScriptDocuments, ScriptStore};
pub use view::{
    AllowAllView, FieldView, document_json, json_to_value, patch_from_json, value_json,
};

/// The DocType every Server Script document is a record of.
pub const SERVER_SCRIPT_DOCTYPE: &str = "server_script";
