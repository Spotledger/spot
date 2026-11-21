//! `spot-plan` — compiled DocType execution plans of the Spotledger Platform
//! (blueprint WS2, WP-MET-02, ADR-05, ADR-16).
//!
//! The document engine must not walk metadata on every write. This crate turns an
//! [`EffectiveMeta`](spot_meta::EffectiveMeta) into a [`DocPlan`] per DocType: everything the
//! engine needs to validate and shape a document, precomputed, immutable and cheap to share.
//!
//! - [`DocPlan::compile`] builds one plan from a DocType's definition and its compiled app
//!   view. It carries a dense [`FieldIx`] per field name (declaration order, engine-managed
//!   fields included), the per-field rule and constraint data, the `links`, `tables` and
//!   `dynamic_links` maps, the unique indexes the storage must hold
//!   ([`DocPlan::unique_indexes`], which include the naming backstop) and a stable
//!   [`PlanHash`] (SHA-256 over a canonical form).
//! - [`DocPlan::check_value`] validates one value against one field: the kind accepts it,
//!   `max_chars`, decimal scale and precision, select membership. Errors are stable
//!   [`plan.*` codes](error::PlanError::code) and name the field.
//! - [`PlanSet::build`] compiles every DocType and reports every problem at once.
//! - [`PlanCache`] holds the current [`Arc`](std::sync::Arc)`<PlanSet>`; `current()` is
//!   lock-free (arc-swap), `refresh()` swaps in a set built from the metadata it is given and
//!   reuses every plan whose hash did not change (the version is information, not the key).
//!
//! # What this crate does not do
//!
//! It does not run a document lifecycle, generate a name or **evaluate** a rule: this crate
//! parses the rule expressions once, at compile time, and validates the dependency graph
//! (`plan.expr.*`, `plan.fetch_from.*`, `plan.naming.*`) — the form layer and the document
//! engine evaluate the compiled rules when a document changes. It reads no storage, no clock,
//! no randomness, and uses no float arithmetic (ADR-12).
//!
//! # Example
//!
//! ```
//! use spot_meta::{Definition, Registry};
//! use spot_plan::{PlanCache, PlanSet};
//!
//! let registry = Registry::new(vec![Definition::new("widget")])
//!     .unwrap_or_else(|e| panic!("definitions do not build: {e:?}"));
//! let set = PlanSet::build(&registry.effective())
//!     .unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
//! let cache = PlanCache::new(set);
//! assert!(cache.current().get("widget").is_some());
//! ```

pub mod cache;
pub mod error;
pub mod hash;
pub mod indexes;
pub mod naming;
pub mod plan;
pub mod rules;
pub mod set;

pub use cache::PlanCache;
pub use error::PlanError;
pub use hash::PlanHash;
pub use indexes::PlanIndex;
pub use naming::{NamingPart, NamingPlan};
pub use plan::{DocPlan, FetchFromPath, FieldIx, PlanField};
pub use rules::{RuleDependencyGraph, evaluation_order, is_mandatory, is_read_only, is_visible};
pub use set::PlanSet;
