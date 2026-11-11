//! Storage semantic parity (WP-STO-10.T1).
//!
//! The rules are written once, in `docs/storage-semantics.md`, each with a stable id. The
//! matrix in `cases/*.toml` proves every rule on the in-memory fake and on PostgreSQL. The
//! [`ParityRunner`] runs the cases on each backend a [`BackendFactory`] makes and produces a
//! [`ParityReport`] that names the rule behind every difference. A difference that is known
//! and accepted is listed in `cases/divergences.toml`; an unlisted one fails the run, and a
//! listed one that has stopped diverging fails it too.

pub mod cases;
pub mod catalog;
pub mod driver;
pub mod dsl;
pub mod exec;
pub mod rules;
pub mod runner;
pub mod values;

pub use cases::{Case, Divergence, EXTENSIONS, Expect, Op, Scope};
pub use driver::{
    BackendFactory, CaseNeeds, Driver, FakeFactory, PgDecision, PgFactory, StorageDriver,
    pg_decision, pg_required,
};
pub use exec::Observed;
pub use rules::{Rule, RuleStatus};
pub use runner::{BackendRun, Difference, ParityReport, ParityRunner};
