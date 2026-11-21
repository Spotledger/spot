//! One cell-text form for geo points and ranges (WP-DOC-09.T7).
//!
//! The import engine converts spreadsheet cells and the CSV/NDJSON export prints values;
//! both use this crate so that export then import round-trips exactly. The forms are cell
//! text only, not a wire format of the native API. No float is used anywhere.
//!
//! - **Point**: `POINT(<lon> <lat>)`, longitude first (OGC well-known text), at most seven
//!   decimal places, a dot, one space between the numbers. Printed with the digits the
//!   `Decimal` holds after `normalize`: `POINT(67.0011 24.8607)`.
//! - **Range**: `[<start>,<end>)`, always half-open. Integers as digits, decimals as
//!   `Decimal::to_string`, dates `YYYY-MM-DD`, instants RFC 3339 UTC with six fractional
//!   digits and `Z`.
//!
//! See the README for refused cells.

mod error;
mod num;
mod point;
mod range;

pub use error::{EXCERPT_CHARS, Reason, TextError};
pub use point::{format_point, parse_point};
pub use range::{format_range, parse_range};
/// The kind of a range field, re-exported so callers need no second dependency for it.
pub use spot_storage_contract::RangeOf;
