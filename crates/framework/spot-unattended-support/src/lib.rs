#![forbid(unsafe_code)]
//! Unattended-support basics (WP-PRD-13.T1): what lets a product with nobody watching it
//! explain itself, report its own crashes and feed the fix pipeline, with no AI in the loop.
//!
//! - [`help`]: an offline help record per error code, bundled into the binary, with
//!   locale-aware lookup and fallback.
//! - [`coverage`] and [`lint`]: the completeness gate (every user-visible code has help, every
//!   `learn_more` document exists, the text follows the style rules) and the skeleton writer.
//!   The `help-coverage` binary runs the same functions.
//! - [`consent`], [`scrub`], [`crash`], [`spool`]: opt-in crash reporting. Nothing is enabled
//!   by default; a report is redacted before it is written; the user can view and delete the
//!   spool.
//! - [`telemetry`]: opt-in aggregate counters, uploaded through an [`Uploader`], with an
//!   inspector that returns exactly the pending payload.
//! - [`intake`]: the vendor side: dedupe by signature, group with a count, rate-limit, and file
//!   a work item through a [`Sink`].
//!
//! Time is always an argument (seconds since the Unix epoch); this crate never reads the clock.

pub mod consent;
pub mod coverage;
pub mod crash;
pub mod error;
pub mod help;
pub mod intake;
pub mod lint;
pub mod scrub;
pub mod spool;
pub mod telemetry;

pub use consent::{Consent, ConsentChange, ConsentKind, ConsentState, ConsentStore};
pub use crash::{CrashCollector, CrashContext, CrashReport};
pub use error::SupportError;
pub use help::{Audience, Help, HelpIndex};
pub use intake::{CrashIntake, IntakeConfig, IntakeOutcome, Sink, WorkItem};
pub use spool::Spool;
pub use telemetry::{Telemetry, TelemetryBatch, Uploader};
