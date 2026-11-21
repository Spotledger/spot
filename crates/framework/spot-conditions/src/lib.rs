//! `spot-conditions`: the conditions framework (WP-SYN-09.T2, ADR-067 sections 1 and 6).
//!
//! A **condition** is a detectable abnormal state with a fixed definition: a stable id, a
//! severity, the kind of subject it is about, the one plain-language sentence that explains it,
//! the automatic response, the escalation schedule and the operator commands that apply. This
//! crate is the generic mechanism, not the sync catalogue: the platform and an app register
//! their conditions through the same API and get the same lifecycle, display and notification.
//!
//! A pure library: no clock read, no I/O, no task, no randomness. Every instant is an argument
//! ([`Millis`]), so the same sequence of calls always produces the same transitions and the same
//! audit events — which is what makes the deterministic simulator of ADR-067 section 7 possible.
//!
//! # The model
//!
//! * [`ConditionDef`]: what a condition is. [`Registry`] holds them and refuses an id it cannot
//!   accept and a duplicate of one it already has.
//! * [`Severity`]: `info`, `amber`, `red`, ordered least to most severe.
//! * [`ConditionId`]: upper snake case (`MISSING_DEP`), checked once so every later use carries a
//!   known-good name.
//! * [`Subject`]: what the condition is about, opaque text compared exactly. A condition is
//!   de-duplicated per `(id, subject)`.
//! * [`ConditionParams`]: the values the explanation template fills in, as text, in name order.
//! * [`Millis`]: an instant or a duration in whole milliseconds, on the caller's clock.
//!
//! # The runtime
//!
//! [`ConditionEngine`] holds what is open. It raises, de-duplicates per subject, debounces
//! against flapping, escalates by elapsed time on the definition's schedule, and clears. Every
//! transition — raised, escalated, cleared — is **exactly one** [`AuditEvent`], handed to an
//! [`AuditSink`]; [`MemorySink`] keeps them in memory. An event is published on one of the two
//! topics ADR-061 section 2 reserves for conditions ([`ConditionTopic`]).
//!
//! [`ConditionEngine::health`] is the "calm by default" surface: counts per severity and the
//! short list that needs a person — every red condition, and every amber one open past the
//! caller's threshold — worst first, then oldest first ([`HealthSummary`]).
//!
//! # Example
//!
//! ```
//! use spot_conditions::{
//!     ConditionDef, ConditionEngine, ConditionParams, MemorySink, Millis, Registry, Severity,
//! };
//!
//! # fn main() -> Result<(), spot_conditions::ConditionsError> {
//! let mut registry = Registry::new();
//! registry.register(ConditionDef::new(
//!     "NODE_SILENT",
//!     Severity::Amber,
//!     "node",
//!     "condition.node_silent",
//! ))?;
//! let mut engine = ConditionEngine::new(registry, MemorySink::new());
//!
//! engine.raise("NODE_SILENT", "branch-12", ConditionParams::new(), Millis::new(0))?;
//! assert_eq!(engine.open().len(), 1);
//!
//! // The node is back. The clear waits out the debounce window, then is recorded once.
//! engine.observe_cleared("NODE_SILENT", "branch-12", Millis::new(5_000))?;
//! engine.tick(Millis::new(65_000));
//! assert!(engine.open().is_empty());
//! assert_eq!(engine.sink().events().len(), 2);
//! # Ok(())
//! # }
//! ```
//!
//! Out of scope for this crate: persistence, notification delivery, the Admin Center health
//! page, and the sync catalogue's own condition entries — the producers that raise them.

mod audit;
mod def;
mod engine;
mod error;
mod health;
mod id;
mod millis;
mod params;
mod registry;
mod severity;
mod subject;

pub use audit::{AuditEvent, AuditSink, ConditionTopic, MemorySink, Transition};
pub use def::{ConditionDef, Escalation, ResponseStep};
pub use engine::{
    ClearOutcome, ConditionEngine, DEFAULT_DEBOUNCE_MILLIS, HistoryEntry, Open, RaiseOutcome,
    TickReport,
};
pub use error::ConditionsError;
pub use health::{HealthSummary, HealthThresholds, SeverityCounts};
pub use id::ConditionId;
pub use millis::Millis;
pub use params::ConditionParams;
pub use registry::Registry;
pub use severity::Severity;
pub use subject::Subject;
