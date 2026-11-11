#![forbid(unsafe_code)]
//! TypeScript DocType controllers (WP-EXT-10.T1; ADR-16, ADR-17, ADR-51, ADR-054).
//!
//! An app ships a TypeScript module (`controllers/invoice.ts`) that is a **full controller** for a
//! DocType: named lifecycle steps and whitelisted methods with typed arguments. This crate is the
//! adapter that turns that module into a [`Controller`](spot_app_api::Controller) — the same trait
//! a native Rust controller implements — so the document engine runs it with no controller-specific
//! code, and the acceptance sentence of the work package holds: *a DocType with a TypeScript
//! controller passes the same lifecycle and API tests as a native one.*
//!
//! # The module contract
//!
//! A module exports:
//!
//! - `contract()`, a function returning the declaration as JSON (`{ doctype, steps, methods }`),
//!   which [`parse_contract`] validates and reports **every** problem of at once;
//! - `run_step({ step, doc, ctx })`, called for each lifecycle step; it may return
//!   `{ patch: {..} }` to change its own document in a `before_*` phase;
//! - `run_method({ method, doc, args, ctx })`, called for each whitelisted method;
//! - a default export holding the handlers, so an editor and the generated `.d.ts` see them
//!   (see [`DTS_HEADER`]).
//!
//! Handlers are synchronous functions over a bounded document view and the global `host` object —
//! the same closed surface Server Scripts expose (WP-EXT-06.T1), plus `host.call_method` for a
//! cross-document method call through the engine as the triggering user. There is no network, no
//! filesystem, no process, no environment and no `eval`.
//!
//! # Loading and running
//!
//! - [`TsControllerFactory`] implements [`ControllerFactory`](spot_controllers::ControllerFactory):
//!   it reads the module from an installed app ([`ModuleSource`]), transpiles it
//!   ([`spot_ts_transpile`]), compiles it on the embedded engine ([`spot_ts`]) and validates the
//!   declaration. Register it with
//!   [`ControllerRegistry::register_from_manifest_ref`](spot_controllers::ControllerRegistry::register_from_manifest_ref);
//!   the registry records it under
//!   [`ControllerSource::TypeScript`](spot_controllers::ControllerSource::TypeScript) and its
//!   method-index rules apply unchanged.
//! - [`TsController`] runs a handler on a fresh runtime per execution, as the triggering user; the
//!   view and every host call go through the step's [`HookContext`](spot_app_api::HookContext) or
//!   the method's [`MethodContext`](spot_app_api::MethodContext).
//! - [`diff_method_decls`] reports what an upgrade changed, so the migration plan (WP-MIG) can
//!   plan around a breaking argument change; the registry's immutable
//!   [`ControllerSet`](spot_controllers::ControllerSet) makes install, upgrade and uninstall a swap
//!   that in-flight operations do not see.
//! - [`controller_conformance`] is the shared suite that proves a controller behaves like a native
//!   one, run here for both and printed as a table of differences.

mod bridge;
mod codes;
mod contract;
mod controller;
mod controller_conformance;
mod diff;
mod error;
mod factory;
mod limits;
mod module;
mod problem;
mod schema;

pub use bridge::{TsBridge, document_from_values, parse_record_id, service_repo, ts_host_surface};
pub use codes::{COMPILE_FAILED, INVALID_MODULE, LIMIT_EXCEEDED, MODULE_MISSING, RESULT_INVALID};
pub use contract::{
    Contract, MethodSpec, StepSpec, hook_point, parse_contract, point_base_priority,
};
pub use controller::TsController;
pub use controller_conformance::{
    Check, ConformanceFixture, ConformanceReport, DenyUsers, run as run_conformance,
};
pub use diff::{MethodChange, MethodChangeKind, diff_method_decls};
pub use error::{compile_failed, is_limit, map_run_error};
pub use factory::{TsControllerFactory, module_error};
pub use limits::TsLimits;
pub use module::{LoadedModule, MemoryModules, ModuleSource, load_in_memory, load_module};
pub use problem::Problem;
pub use schema::{
    ArgsSchema, BAD_ARGUMENTS, FieldType, TypeTag, parse_tag, result_schema_from_json, tag_schema,
    validate_args, validate_result,
};

/// The `.d.ts` header the typed client and the editor integration prepend to a generated
/// DocType declaration, documenting the controller module contract a TypeScript author writes
/// against.
pub const DTS_HEADER: &str = r#"// A Spotledger TypeScript DocType controller (WP-EXT-10.T1).
//
// A controller module exports:
//   contract()                       -> { doctype, steps, methods } (the declaration)
//   run_step({ step, doc, ctx })     -> void | { patch: Record<string, unknown> }
//   run_method({ method, doc, args, ctx }) -> unknown
// and default-exports the handlers, so the editor sees them.
//
// Steps declare a hook point (before_save, validate, after_save, before_submit, on_submit,
// before_cancel, on_cancel, after_commit, load, field_changed:<field>) and an optional priority.
// Only a before_* step may return a patch; the engine refuses one anywhere else.
//
// Methods declare the right they need (read, write, submit, cancel, create, or a permission) and
// their arguments and result as type tags: text, int, decimal, bool, date, datetime, time, id,
// json, any, each optionally `?` (nullable) and `[]` (an array). A decimal is its exact string,
// never a number. Unknown arguments are refused.
//
// `ctx` is the closed host surface (host.log/get/query/insert/update/call/throw/format_money/
// round, and host.call_method), run as the triggering user."#;
