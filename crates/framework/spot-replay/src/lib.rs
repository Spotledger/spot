//! `spot-replay` — behavioural replay of recent transactions through the new release on the clone
//! (blueprint WS12, WP-MIG-06.T2, ADR-22).
//!
//! Migration runs **plan → approve → rehearse → apply → verify**. WP-MIG-06.T1's rehearsal
//! measures the clone's *fingerprint* before and after the plan; this crate is the second half:
//! it takes the last N days (or N documents) of real transactions from the site's journal,
//! re-executes each one through the **new** release on the clone, and reports every outcome that
//! differs from what the old release recorded.
//!
//! # What a replay does
//!
//! 1. A [`ReplaySource`] reads committed operations from the site's event journal / outbox
//!    (ADR-061), each with the [`Outcome`] the **old** release recorded.
//! 2. [`select`] draws a [`Sample`] from them under the site's [`ReplayPolicy`]: a window of the
//!    last N days or N documents, stratified by `(DocType, sensitivity)` and deterministic given
//!    a seed.
//! 3. For each sampled operation, a [`ReplayRunner`] re-runs it through the document engine
//!    **on the clone only**, with the clock fixed to the original instant, the original
//!    [`Principal`]'s rights, and every external effect passing through the [`EffectHold`]
//!    engaged in `Held` mode.
//! 4. [`diff`] compares the recorded and the re-executed [`Outcome`] field by field. A difference
//!    of exactly one minor unit is flagged [`rounding`](Divergence::rounding) — the signature of
//!    a changed rounding rule, which the acceptance test seeds.
//! 5. Every divergence a declared behaviour entry (WP-MIG-02) claims is
//!    [`Expected`](DivergenceClass::Expected); the rest are
//!    [`Unexplained`](DivergenceClass::Unexplained). The result is a [`ReplayReport`], structured
//!    (JSON) and rendered ([`ReplayReport::render_text`]).
//!
//! # Bounds
//!
//! A [`Budget`] caps the documents and the duration (measured on the injected
//! [`Clock`](spot_core::Clock), never the wall clock). A run that hits the cap stops cleanly,
//! reports its [`Coverage`] and leaves a [`Cursor`] a later [`Replay::resume`] continues from.
//! Ordering is deterministic: causal order within a source, sources in a fixed order.
//!
//! # Honest limits
//!
//! - Nothing here touches the live site database: the only write target is the
//!   [`CloneRef`] T1's rehearsal created.
//! - Nothing leaves the clone: every external effect goes through the hold, and the one class the
//!   hold would let through is refused ([`Error::EffectEscaped`]).
//! - No floating-point arithmetic for money or quantities (ADR-12): every measure is an integer
//!   count of minor units.
//! - The engine reads no clock of its own: business time and the reference instant are arguments,
//!   and the duration budget reads the injected monotonic clock.
//! - The engine contains no ERP DocType, account or party names.

pub mod budget;
pub mod clone;
pub mod codes;
pub mod declaration;
pub mod divergence;
pub mod engine;
pub mod error;
pub mod hold;
pub mod journal;
pub mod outcome;
pub mod policy;
pub mod report;
pub mod runner;

pub use budget::{Budget, BudgetStop};
pub use clone::CloneRef;
pub use declaration::{BehaviourEntry, DeclaredBehaviour, DeclaredBehaviourList, NoDeclarations};
pub use divergence::{Divergence, DivergenceClass, DivergenceKind, diff};
pub use engine::Replay;
pub use error::{Error, Result};
pub use hold::EffectHold;
pub use journal::{
    Cursor, JournalEntry, JournalPage, OperationKind, Principal, ReplayOperation, ReplaySource,
    Sensitivity,
};
pub use outcome::{LedgerEntry, Outcome, Status, StockEntry, ValidationError};
pub use policy::{ReplayPolicy, Sample, Stratum, Window, select};
pub use report::{Coverage, ReplayOutcome, ReplayReport};
pub use runner::{ReplayRequest, ReplayRunner};

/// A boxed future: what an object-safe async method returns.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
