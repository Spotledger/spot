//! Reading the naming strategy out of a compiled plan, and rendering a series name.
//!
//! The pattern was parsed once by [`spot_plan`]; this module only interprets the compiled
//! [`NamingPart`]s. It never re-parses pattern text.

use std::sync::Arc;

use spot_plan::{NamingPart, NamingPlan};
use time::Date;

use crate::error::NamingError;
use crate::fiscal::FiscalYear;
use crate::{FISCAL_YEAR_PART, PROMPT};

/// The strategy a compiled naming plan asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// A series pattern: literals, date parts and a counter taken from a series.
    Series,
    /// A lone `field:<name>` part: the name is that field's value.
    Field(String),
    /// A lone `hash` part: a short id derived from a UUIDv7.
    Hash,
    /// The literal `prompt`: the caller supplies the name.
    Prompt,
}

impl Strategy {
    /// A short name for logs and diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Series => "series",
            Self::Field(_) => "field",
            Self::Hash => "hash",
            Self::Prompt => "prompt",
        }
    }
}

/// Reads the strategy from a compiled plan.
///
/// # Errors
///
/// [`NamingError::PatternInvalid`] for a plan that is none of the four strategies: a `field:`
/// part mixed with other text, a `hash` part with neighbours, or literals with no counter.
pub(crate) fn detect(doctype: &str, plan: &NamingPlan) -> Result<Strategy, NamingError> {
    let invalid = || NamingError::PatternInvalid {
        doctype: doctype.to_owned(),
    };
    match plan.parts.as_slice() {
        [NamingPart::Field(name)] => Ok(Strategy::Field(name.clone())),
        [NamingPart::Hash] => Ok(Strategy::Hash),
        [NamingPart::Literal(text)] if text == PROMPT => Ok(Strategy::Prompt),
        parts if parts.iter().any(is_counter) => {
            if parts.iter().all(is_series_part) {
                Ok(Strategy::Series)
            } else {
                Err(invalid())
            }
        }
        _ => Err(invalid()),
    }
}

fn is_counter(part: &NamingPart) -> bool {
    matches!(part, NamingPart::Counter { .. })
}

fn is_series_part(part: &NamingPart) -> bool {
    matches!(
        part,
        NamingPart::Literal(_)
            | NamingPart::Year { .. }
            | NamingPart::Month
            | NamingPart::Day
            | NamingPart::Counter { .. }
    )
}

/// A series plan bound to one business date: renders the prefix and one counter value.
#[derive(Clone)]
pub(crate) struct SeriesFormat {
    parts: Vec<NamingPart>,
    date: Date,
    fiscal: Arc<dyn FiscalYear>,
}

impl SeriesFormat {
    /// Binds a plan's parts to a date and the fiscal-year resolver.
    pub(crate) fn new(parts: Vec<NamingPart>, date: Date, fiscal: Arc<dyn FiscalYear>) -> Self {
        Self {
            parts,
            date,
            fiscal,
        }
    }

    /// The resolved prefix: every part except the counter, so it identifies the series.
    ///
    /// # Errors
    ///
    /// [`NamingError::PatternInvalid`] if a non-series part slipped in (it cannot after
    /// [`detect`], which is defensive).
    pub(crate) fn prefix(&self) -> Result<String, NamingError> {
        self.render(None)
    }

    /// The whole name for `number`, or the prefix when `number` is `None`.
    ///
    /// # Errors
    ///
    /// [`NamingError::PatternInvalid`] if a non-series part slipped in.
    pub(crate) fn render(&self, number: Option<u64>) -> Result<String, NamingError> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                NamingPart::Literal(text) if text == FISCAL_YEAR_PART => {
                    out.push_str(&format!("{:04}", self.fiscal.fiscal_year(self.date)));
                }
                NamingPart::Literal(text) => out.push_str(text),
                NamingPart::Year { width } => out.push_str(&render_year(self.date.year(), *width)),
                NamingPart::Month => out.push_str(&format!("{:02}", u8::from(self.date.month()))),
                NamingPart::Day => out.push_str(&format!("{:02}", self.date.day())),
                NamingPart::Counter { width } => {
                    if let Some(number) = number {
                        out.push_str(&render_counter(number, *width));
                    }
                }
                NamingPart::Field(_) | NamingPart::Hash => {
                    return Err(NamingError::PatternInvalid {
                        doctype: String::new(),
                    });
                }
            }
        }
        Ok(out)
    }
}

/// A year part: `YYYY` prints the year; a narrower part prints its low digits.
fn render_year(year: i32, width: u8) -> String {
    let width = usize::from(width.max(1));
    if width >= 4 {
        return format!("{year:0width$}");
    }
    let modulus = 10_i64.pow(u32::try_from(width).unwrap_or(4));
    let low = i64::from(year).rem_euclid(modulus);
    format!("{low:0width$}")
}

/// A counter part: zero-padded to its width, widening rather than truncating on overflow.
fn render_counter(number: u64, width: u8) -> String {
    format!("{:0width$}", number, width = usize::from(width.max(1)))
}
