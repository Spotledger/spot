//! spot-mgmt-ops — the typed management operation catalogue (WP-ADM-01.T1, blueprint
//! WS22, ADR-44).
//!
//! The Admin Center console and the `spot` CLI are equal clients of the management API,
//! and "every GUI action can show its equivalent command". That only holds if an operation
//! is **data**: a name, typed parameters and a risk level, from which both the API
//! validation and the command line are derived. This crate holds that data model and
//! nothing else — no HTTP, no TLS, no execution, no signing (separate tasks).
//!
//! # Shape
//!
//! - [`model`]: the data: [`OperationSpec`] with [`OpId`], [`Risk`], [`TargetKind`],
//!   [`ParamSpec`], [`ParamType`] and [`ParamValue`]; all serde; `Risk` ordered
//!   `Read < Low < Medium < High < Critical`.
//! - [`catalogue`]: [`Catalogue`] and [`CatalogueBuilder`]: duplicate ids (naming both
//!   modules), duplicate parameters, invalid defaults, secret-with-default, invalid
//!   regexes and inverted `Int` ranges are all refused at build time; `get`, `iter` in
//!   sorted id order and `search` over id, title and summary.
//! - [`validate`]/[`validated`]: [`Catalogue::validate`] turns an [`Invocation`] into a
//!   [`Validated`] (defaults filled), reporting every problem at once.
//! - [`render`]: `render(&Validated, Shell)` writes the CLI line — segments become
//!   subcommands, parameters become `--kebab-case` flags, POSIX and PowerShell quoting,
//!   secrets rendered only as `--password <prompt>`.
//! - [`parse`]: the parser that reads exactly what `render` writes; the round trip
//!   `parse(render(x)) == x` is a tested property.
//! - [`module`]: [`OperationModule`], the way the Linux, Windows and database modules
//!   register their operations.
//!
//! # Purity
//!
//! No storage, no clock, no network, no ambient reads, no floats, no `unsafe` (the project
//! coding rules). The shell round-trip test shells out (`sh -c` / `powershell`), which is a
//! test-side check of the quoting rules, not crate behaviour.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod catalogue;
pub mod error;
pub mod model;
pub mod module;
pub mod parse;
pub mod render;
pub mod validate;
pub mod validated;
pub mod value_check;

pub use catalogue::{Catalogue, CatalogueBuilder};
pub use error::{CatalogueError, InvocationError};
pub use model::{
    OpId, OpIdError, OperationSpec, ParamSpec, ParamType, ParamValue, Risk, TargetKind,
};
pub use module::OperationModule;
pub use parse::ParseError;
pub use render::{Shell, argv, quote, render};
pub use validated::{AcceptedParam, Invocation, SECRET_PLACEHOLDER, Validated};

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-mgmt-ops";
