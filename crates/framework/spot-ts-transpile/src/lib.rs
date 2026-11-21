//! Spotledger TypeScript-to-JavaScript transpiler.
//!
//! ADR-054 step 2: on save, TypeScript is stripped to plain JavaScript that
//! the engine tier (QuickJS via `rquickjs`) runs. This crate is the only
//! boundary: type *checking* is the editor integration (WP-EXT-09), engine
//! embedding is `spot-ts` (WP-EXT-08.T1); both consume this crate's output.
//!
//! - [`transpile::transpile`] — parse, check the script-tier rules, strip
//!   types, emit JavaScript plus a standard v3 source map, deterministically.
//! - [`diagnostic::Diagnostic`] — every failure, positioned (1-based).
//! - [`options::Options`] — the script-tier knobs (import allow-list, caps).

pub mod collect;
/// Failure type plus the 1-based position index.
pub mod diagnostic;
/// Input limits: the save-time caps.
pub mod limit;
/// Knobs the script tier exposes to the caller.
pub mod options;
/// The transpile flow.
pub mod transpile;

pub use diagnostic::Diagnostic;
pub use options::Options;
pub use transpile::{Import, ImportedName, Transpiled, transpile};
