//! `spot-rule-graph` — business rules as Datalog over the graph (WP-GRA-04.T1, ADR-50).
//!
//! Roll-ups, approver resolution and affected-item analysis are **derived relationships** over
//! the business graph, written as data and compiled like metadata, not hand-coded traversals.
//! This crate is the rule language front end, its compiler and validator, a deterministic
//! evaluator, and the registry that hot-swaps rule sets by content hash.
//!
//! # The language
//!
//! A program is a sequence of rules `head :- body.`. A **predicate** is one of:
//!
//! - an **edge type** of the graph model (WP-GRA-01), named by its canonical name
//!   (`invoice.grn`, `sales_order_contains_item`): two arguments, both record keys, in storage
//!   orientation `(holder, target)`;
//! - a **field predicate**, `doctype.field` for a field that is not a link or a table
//!   (`account.amount`): two arguments, `(record, value)`;
//! - a **derived predicate**, defined by a rule head.
//!
//! A body is a comma-separated list of literals: a positive atom, a negated atom (`not
//! pred(...)`) or a guard (`where (expr)`). A guard is a [shared expression][spot_expr] over
//! the rule's bound variables, read as `doc.<variable>`; `now()` is the query's instant.
//! A head argument is a variable, a constant, or an aggregate `count<X>`, `sum<X>`, `min<X>`,
//! `max<X>`. `sum` is exact (integer or decimal, never a float); `count` counts distinct values.
//!
//! Rules only derive and read: the language has no write form.
//!
//! # Termination
//!
//! A program is accepted only when it is **range-restricted** — every variable in the head, a
//! negated atom or a guard is bound by a positive body atom — and **stratifiable** — no cycle
//! runs through negation or an aggregate. Those two checks, with a finite fact source, make the
//! least fixed point finite; a fuel limit ([`DEFAULT_FUEL`]) turns a program that would still
//! derive too much into [`RuleGraphError::FuelExhausted`] rather than run forever.
//!
//! # Determinism and hot reload
//!
//! Evaluation is a pure function of the compiled program, the [`FactSource`] and the query's
//! instant; output is sorted and deduplicated, so no hash-map order leaks. A [`Compiled`]
//! program carries a SHA-256 [`ContentHash`]; a [`Registry`] swaps a set atomically when its
//! content changes, and [`Registry::reload`] recompiles every set against a new effective
//! metadata — the hook the metadata live-reload signal drives.
//!
//! # Decision: an interpreter, not `ascent`
//!
//! `ascent!` is a compile-time macro, so a rule loaded at runtime cannot reach it. The
//! blueprint's "`ascent` first choice" is guidance; the binding behaviour is a rule corpus with
//! deterministic results and a rule change that takes effect without restart. This crate
//! therefore embeds a small semi-naive Datalog evaluator of its own over the graph model's
//! relations (the blueprint's option (a)).
//!
//! # What this crate is not
//!
//! It holds no storage, no permissions and no clock (time is an argument); it reads no
//! randomness and uses no `unsafe` or floats. The in-memory graph index adapter is the
//! `graph-index` feature; a storage-backed [`FactSource`] is a caller's implementation of the
//! same trait.
//!
//! # Example
//!
//! ```
//! use spot_meta::{Definition, FieldSpec, FieldType, Registry};
//! use spot_rule_graph::{Engine, MemoryFacts, Query, RuleSet};
//!
//! // A DocType with a self link and an amount.
//! let mut account = Definition::new("account");
//! let mut parent = FieldSpec::new("parent_account", FieldType::Link);
//! parent.options = Some("account".to_owned());
//! account.fields.push(parent);
//! let mut amount = FieldSpec::new("amount", FieldType::Decimal);
//! amount.scale = Some(2);
//! account.fields.push(amount);
//! let meta = Registry::new(vec![account])
//!     .map(|r| r.effective())
//!     .unwrap_or_else(|errors| panic!("metadata builds: {errors:?}"));
//!
//! // Sum the amounts of every account in each account's subtree.
//! let source = "
//! descendant(A, A) :- account.amount(A, _).
//! descendant(C, A) :- account.parent_account(C, P), descendant(P, A).
//! subtree_total(A, sum<Amount>) :- descendant(D, A), account.amount(D, Amount).
//! ";
//! let compiled = RuleSet::compile(source, &meta).unwrap_or_else(|e| panic!("compiles: {e}"));
//! let facts = MemoryFacts::new();
//! let rows = Engine::new()
//!     .evaluate(&compiled, &facts, &Query::new("subtree_total"))
//!     .unwrap_or_else(|e| panic!("evaluates: {e}"));
//! assert!(rows.is_empty());
//! ```
#![forbid(unsafe_code)]

pub mod ast;
pub mod compile;
pub mod datum;
pub mod error;
pub mod eval;
pub mod parse;
pub mod registry;
pub mod source;

#[cfg(feature = "graph-index")]
pub mod graph_facts;

pub use ast::{AggFunc, Const, Head, HeadArg, Literal, Program, Rule, Span, Term};
pub use compile::{Compiled, ContentHash, PredKind, Predicate, RuleSet};
pub use datum::Datum;
pub use error::{Problem, RuleGraphError};
pub use eval::{DEFAULT_FUEL, Engine, Query, Rows, Tuple};
pub use registry::{Registry, Reload, ReloadFailure};
pub use source::{FactSource, MemoryFacts};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-rule-graph";
