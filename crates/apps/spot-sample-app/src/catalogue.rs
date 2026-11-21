//! The app's typed codes and messages (WP-DX-05.T1): the vocabulary of
//! `sample.hours_out_of_range`, `sample.hours_required` and
//! `sample.owner_from_project`, catalogued in
//! `crates/framework/spot-errors/catalogue/sample.toml` (raise sites live in
//! [`crate::controller`]).

/// The Time Entry hours are outside 0.25–24 (a field error on `hours`).
pub const HOURS_OUT_OF_RANGE: &str = "sample.hours_out_of_range";

/// A Time Entry save carries no hours value at all; it stays a plain
/// `document.required` refusal from the host, so this code is a message, not an
/// error: the controller never raises it.
pub const HOURS_REQUIRED: &str = "sample.hours_required";

/// A Task save derives `owner` from the linked Project's owner (an information
/// message the before-save step adds when it fills the field).
pub const OWNER_FROM_PROJECT: &str = "sample.owner_from_project";

/// The hours bounds, exact decimals (ADR-12): (lowest, highest), inclusive.
pub const HOURS_BOUNDS: (&str, &str) = ("0.25", "24.00");
