//! `spot-sync-healing`: the self-healing repair driver (WP-SYN-09.T4, ADR-067 sections 3, 4
//! and 6).
//!
//! Reconciliation (`spot-sync-reconcile`) finds divergence and *suggests* repairs. This crate
//! executes them safely. It is a library over traits, with virtual time: no network, no
//! database, no clock, no randomness, no task.
//!
//! * [`HealingDriver::step`] takes the latest [`Observation`]s and `now`, converts each to
//!   repairs through `plan_repairs`, orders them (most urgent first, then oldest), and runs at
//!   most `max_in_flight` of them within the global and per-node one-minute rates, through
//!   [`RepairActions`]. Every action is written to a [`HealingJournal`] (keys and versions only,
//!   never record content).
//! * A failed repair backs off (doubling, capped). After `max_attempts` executions without the
//!   finding disappearing it stops, and [`CONDITION_ID`] is raised with the journal as
//!   evidence. A clean comparison clears it. The host calls [`register_conditions`] once; the
//!   definition moves to the `spot-sync-conditions` catalogue when the ADR-067 row exists
//!   (WP-SYN-09.T5).
//! * A scope on the freeze list ([`FrozenScopes`]) is never repaired and shows as `Frozen` in
//!   [`HealingDriver::status`].
//! * **Direction.** Roles decide: only an `Owner` source to a `Mirror` destination is repaired.
//!   A missing role, a mirror as source, a mirror newer than its owner: no repair, and the
//!   condition is raised.
//!
//! # Example
//!
//! ```
//! use spot_conditions::Millis;
//! use spot_sync_healing::{
//!     ActionOutcome, ActionRequest, HealingDriver, HealingSettings, MemoryJournal, RepairActions,
//! };
//!
//! struct Nothing;
//! impl RepairActions for Nothing {
//!     fn rebuild_digest(&mut self, _: &ActionRequest<'_>) -> ActionOutcome { ActionOutcome::Done }
//!     fn repull_record(&mut self, _: &ActionRequest<'_>) -> ActionOutcome { ActionOutcome::Done }
//!     fn resend_scope(&mut self, _: &ActionRequest<'_>) -> ActionOutcome { ActionOutcome::Done }
//!     fn request_snapshot(&mut self, _: &ActionRequest<'_>) -> ActionOutcome { ActionOutcome::Done }
//! }
//!
//! # fn main() -> Result<(), spot_sync_healing::HealingError> {
//! let mut driver = HealingDriver::new(Nothing, MemoryJournal::new(), HealingSettings::default())?;
//! assert!(driver.step(&[], Millis::new(0)).is_empty());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod actions;
mod driver;
mod error;
mod frozen;
mod journal;
mod plan;
mod settings;
mod signal;
mod window;

pub use actions::{ActionKind, ActionOutcome, ActionRequest, RepairActions, Target};
pub use driver::{Attempt, HealingDriver, RepairStatus, StatusEntry};
pub use error::HealingError;
pub use frozen::{FrozenScopes, MemoryFrozen, NoFrozen};
pub use journal::{HealingJournal, JournalEntry, JournalOutcome, MemoryJournal};
pub use plan::{Observation, Priority};
pub use settings::{HealingSettings, RATE_WINDOW};
pub use signal::{
    CONDITION_ID, Cause, ConditionSignal, SUBJECT_KIND, apply_signals, condition_def,
    register_conditions,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-healing";
