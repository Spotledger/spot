//! The report engine (WP-PLT-14.T1, ADR-45, ADR-069, ADR-070 section 12).
//!
//! A **report** is a named, declared query: columns, filters with typed parameters, grouping,
//! aggregates, ordering and a row limit, defined as data (TOML). It runs as **one storage
//! query** with the caller's permission filter ANDed on, and returns rows the caller may see
//! in columns the caller may report on. There is no free SQL anywhere: the only query language
//! is the storage query IR.
//!
//! - [`definition`]: the TOML model ([`ReportDef`](definition::ReportDef)) and its semantic
//!   validation, reporting every problem at once.
//! - [`compile`]: the [`Compiler`](compile::Compiler) resolves every field against the
//!   effective metadata, type-checks parameters, builds the IR [`Query`](spot_storage_contract::Query)
//!   and validates it under ADR-069's limits, producing a
//!   [`CompiledReport`](compile::CompiledReport).
//! - [`runner`]: the [`ReportRunner`](runner::ReportRunner) binds arguments, refuses a query
//!   that would infer through a field the caller cannot read, ANDs the permission filters,
//!   reduces the columns for [`Channel::Report`](spot_field_guard::Channel::Report), executes
//!   through a [`QueryExecutor`](runner::QueryExecutor) and returns the contract's
//!   [`ReportResult`](spot_http::api::service::ReportResult).
//! - [`registry`]: reports registered per app, listed for a principal and looked up by name.
//! - [`service`]: the native API's
//!   [`ReportService`](spot_http::api::service::ReportService) implementation.
//! - [`cache`]: compiled reports cached by `(name, metadata identity)` in a bounded map, where
//!   the identity is the metadata version together with a content digest of the DocTypes.
//!
//! Prepared reports (stored results) are WP-PLT-14.T2; the report builder UI is a UI-stage
//! task.

pub mod cache;
pub mod compile;
pub mod compiled;
pub mod definition;
pub mod encode;
pub mod error;
pub mod meta;
pub mod registry;
pub mod runner;
pub mod service;
pub mod walk;

pub use cache::ReportCache;
pub use compile::Compiler;
pub use compiled::{
    CREATED_BY, ColumnOrigin, CompiledColumn, CompiledComputed, CompiledFilter, CompiledJoin,
    CompiledOrder, CompiledParameter, CompiledReport, FieldPath, ParamOptions, Rhs,
};
pub use definition::{
    AggKind, ColumnDef, CompareOp, ComputedDef, FilterNode, JoinDef, OrderDef, ParamType,
    ParameterDef, ReportDef,
};
pub use error::{ReportError, codes};
pub use meta::{EffectiveMetaSet, MetaIdentity};
pub use registry::{RegisteredReport, ReportRegistry};
pub use runner::{
    BoxFuture, NoTelemetry, QueryExecutor, ReportArgs, ReportRunner, RunOutput, Telemetry,
};
pub use service::{
    AnonymousPrincipals, PrincipalSource, ReportApiService, ReportResourceService, ReportService,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-reports";
