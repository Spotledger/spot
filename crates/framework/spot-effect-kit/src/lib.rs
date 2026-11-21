//! The effect conformance kit (WP-EXT-11.T3; ADR-080 sections 6.3 to 6.5).
//!
//! An app declares what its pipeline steps do to the world (contract 0.5.0) and the engine
//! gathers what they report into receipts. This crate makes those rules enforceable by the app's
//! own test suite, like the other test kits:
//!
//! ```ignore
//! let report = spot_effect_kit::run(&factory, &scenarios).await?;
//! report.assert_clean();
//! ```
//!
//! For each [`Scenario`] (a DocType and a way to make a valid draft of it) the kit asks the
//! app's [`EngineFactory`] for a fresh engine, wraps the controllers with a [`Recorder`] so
//! reports are kept, then drives **create, post, cancel, a second cancel and correct**, taking a
//! reading of every store through the app's [`StoreProbe`] around each. It reports:
//!
//! - `effect_kit.not_netted_to_zero` / `effect_kit.over_reversal`: after post then cancel a
//!   store's aggregate is not what it was before (under- or over-reversed);
//! - `effect_kit.row_deleted`: a store has fewer rows than before, since stores are append-only;
//! - `effect_kit.unreported_write` / `effect_kit.over_reported`: the rows a store gained differ
//!   from what the operation's receipt reports;
//! - `effect_kit.undeclared_report`: a receipt holds an effect no declaration covers
//!   ([`spot_seams::check_receipt`]);
//! - `effect_kit.reversal_unlinked`: the reversing rows do not reference the posted ones (only
//!   when the probe records the link);
//! - `effect_kit.cancel_not_refused` / `effect_kit.cancel_duplicated`: cancelling again must be
//!   refused by the lifecycle and change nothing;
//! - `effect_kit.correct_not_cancel_plus_new`: *correct* must be cancel plus a new linked draft,
//!   leaving the stores as they were.
//!
//! Every finding names the step, store and delta and says how to fix it. Time is an argument
//! (the scenario's [`Invocation`](spot_app_api::Invocation)); the kit never reads a clock.

mod conformance;
mod failure;
mod probe;
mod recorder;

pub use conformance::{EngineFactory, Scenario, World, run};
pub use failure::{Report, Violation, codes};
pub use probe::{StoreFilter, StoreProbe};
pub use recorder::Recorder;
