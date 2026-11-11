//! The controller registry (WP-DOC-10.T1; ADR-16, ADR-17, ADR-51).
//!
//! A DocType's behaviour is its controller: named pipeline steps on the ADR-51 hook points and
//! whitelisted methods callable on one document. Apps register controllers into
//! [`ControllerRegistry`]; the registry answers which controllers a DocType has, which
//! whitelisted methods it dispatches, and — through [`ControllerSet`] — attaches every
//! controller's steps to [`spot_seams`] so the document engine needs no controller-specific
//! code.
//!
//! # Registering
//!
//! ```no_run
//! use std::sync::Arc;
//! use spot_controllers::{ControllerRegistry, ControllerSource};
//! # use spot_app_api::{AppResult, BoxFuture, Controller, HookContext, MethodDecl, StepDecl, StepName};
//! # struct MyController;
//! # impl Controller for MyController {
//! #     fn doctype(&self) -> &str { "note" }
//! #     fn steps(&self) -> Vec<StepDecl> { Vec::new() }
//! #     fn run_step<'a>(&'a self, _s: &'a StepName, _c: &'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> { Box::pin(async { Ok(()) }) }
//! # }
//! let mut registry = ControllerRegistry::new();
//! registry.register("my_app", Arc::new(MyController), ControllerSource::Native);
//! let set = registry.set();                       // the immutable snapshot to hand to the engine
//! let controllers = set.controllers_for("note");  // one Registered
//! ```
//!
//! # Three kinds, one trait
//!
//! Native Rust, WASM and TypeScript controllers implement the same
//! [`Controller`](spot_app_api::Controller) trait; the engine treats them alike. The
//! [`ControllerSource`] is recorded for diagnostics and the admin listing only. The WASM and
//! TypeScript **adapters** are WP-EXT-10 and the WASM host work; until they land, a factory
//! answers their references with `controller.host_unavailable`, naming the missing adapter.
//!
//! # The default controller
//!
//! Every DocType resolves to *some* controller. An app's controller when one is registered, and
//! [`DefaultController`] — no steps, no methods — otherwise, including for a DocType that did
//! not exist when the registry was built. That is what lets a DocType created at runtime save.
//!
//! # Installing
//!
//! [`ControllerSet::install_into`] turns the controllers' steps into [`SeamRegistry`]
//! registrations, with the app manifest's priority and budget when it names a step.
//! [`ControllerSet::install`] builds a fresh [`spot_seams::Registry`] and reports **every**
//! conflict at once. Registering or unregistering produces a new [`ControllerSet`] behind an
//! `Arc`, so an operation that began on one set finishes on it.

mod codes;
mod default;
mod factory;
mod install;
mod registry;
mod source;

pub use codes::{
    DOCTYPE_DUPLICATE, EVENT_DOCTYPE_UNSERVED, HOST_UNAVAILABLE, INVALID_REFERENCE,
    METHOD_CONFLICT, NO_STEP, UNKNOWN_NATIVE,
};
pub use default::DefaultController;
pub use factory::{ControllerFactory, NativeControllers};
pub use registry::{
    ControllerRegistry, ControllerSet, DEFAULT_APP, MethodIndex, MethodInfo, Registered, Right,
};
pub use source::{ControllerRef, ControllerSource};

/// The seam registry a controller set installs into (`spot-seams`).
pub use spot_seams::RegistryBuilder as SeamRegistry;
