#![forbid(unsafe_code)]
//! Exact money and quantities, with rounding rules as data (WP-DOC-02.T1, ADR-12).
//!
//! **No binary floating point, anywhere.** Values are `rust_decimal` decimals, but this crate does
//! not trust the decimal type's own arithmetic where it would round silently: addition,
//! multiplication, rounding, allocation and tax work on `i128` mantissas and exact fractions, and
//! a result that does not fit is an error ([`MoneyError::Overflow`], [`MoneyError::Inexact`]), never
//! a wrong number. `tests/no_floats.rs` scans `src/` for the two float type names.
//!
//! # What is here
//!
//! - [`Money`] (a decimal and a [`Currency`]) and [`Quantity`] (a decimal and an optional [`Uom`]):
//!   different types, so a quantity cannot be added to money; currencies and units of measure are
//!   never mixed (`money.currency_mismatch`, `money.uom_mismatch`).
//! - [`CurrencyTable`]: the minor-unit exponent of each currency, supplied as data. There is no
//!   built-in list.
//! - [`RoundingRule`] (`scale`, `mode`, optional `step`) and [`Mode`]: see [`rounding`] for the
//!   exact meaning of the seven modes.
//! - [`RuleSet`]: named rules bound to `(kind, key)` pairs from TOML, with a documented
//!   fallback order and [`RuleSet::explain`].
//! - [`mul`], [`percent`], [`sum_decimals`] and [`allocate`] (largest remainder: the parts sum to
//!   the total exactly).
//! - [`tax`]: inclusive/exclusive conversion, and per-line or on-total rounding.
//!
//! Rounding is never implied: an operation either returns an exact result or takes the rule it
//! rounds by.

pub mod amount;
pub mod currency;
pub mod error;
mod exact;
pub mod ops;
pub mod rounding;
pub mod ruleset;
pub mod tax;

pub use amount::{Money, Quantity, sum_decimals};
pub use currency::{Currency, CurrencyTable, Uom};
pub use error::{MoneyError, Result};
pub use ops::{allocate, allocate_decimals, mul, mul_exact, percent, percent_rounded};
pub use rounding::{Mode, RoundingRule, parse_decimal};
pub use ruleset::{Explanation, Kind, Problem, RuleSet, RuleSetError, Source};
pub use tax::{
    RoundingScope, TaxLines, exclusive_lines, inclusive_lines, tax_exclusive_to_inclusive,
    tax_in_inclusive, tax_inclusive_to_exclusive, tax_on_exclusive,
};
