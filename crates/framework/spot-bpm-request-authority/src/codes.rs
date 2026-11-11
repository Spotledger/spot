//! The stable error codes this crate raises, each with one entry in
//! `spot-errors/catalogue/bpm_request_authority.toml`.
//!
//! They are `pub const`s so the catalogue scan finds them and a caller can match on them without
//! a string literal.

/// A client-supplied command id was shaped like a request grant (`rg1.…` or the old plain
/// `request:node:attempt`), so the edge refused it instead of treating it as an idempotency key.
pub const COMMAND_ID_RESERVED: &str = "bpm_request_authority.command_id_reserved";

/// A grant was asked for with a blank request, step or acting person, so it would bind nothing.
pub const IDENTIFIER_BLANK: &str = "bpm_request_authority.identifier_blank";

/// A grant was asked for with an expiry that is already past or further away than the 15-minute
/// cap.
pub const LIFETIME_TOO_LONG: &str = "bpm_request_authority.lifetime_too_long";

/// A grant was asked for an attempt the request store does not hold as the current one, or for a
/// request or step that is not open.
pub const ATTEMPT_NOT_CURRENT: &str = "bpm_request_authority.attempt_not_current";

/// The server secret a grant is signed under could not be resolved from the vault, or is shorter
/// than the minimum this crate accepts.
pub const SECRET_UNAVAILABLE: &str = "bpm_request_authority.secret_unavailable";

/// A grant was asked for with an acting person that is not a typed principal reference
/// (`user:<id>`, `consumer:<id>` or `system`), so the same id under two kinds could be confused
/// (ADR-083 section 2.8 item 4).
pub const ACTOR_NOT_TYPED: &str = "bpm_request_authority.actor_not_typed";
