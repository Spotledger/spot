//! The fiscal-year resolver: which fiscal year a business date belongs to.
//!
//! The Foundation's fiscal calendar is not built yet (build order, ADR-060), so the resolver
//! is injected. [`CalendarFiscalYear`] is the plain calendar year; a test or a later Foundation
//! crate supplies a fiscal start.

use time::Date;

/// Maps a business date to its fiscal year label.
pub trait FiscalYear: Send + Sync {
    /// The fiscal year a business date belongs to, as the number a name prints (for example
    /// `2026`).
    fn fiscal_year(&self, date: Date) -> i32;
}

/// The plain calendar year: the fiscal year equals the date's year.
#[derive(Clone, Copy, Debug, Default)]
pub struct CalendarFiscalYear;

impl FiscalYear for CalendarFiscalYear {
    fn fiscal_year(&self, date: Date) -> i32 {
        date.year()
    }
}

/// A fiscal year that starts on `start_month`/`start_day` of the calendar year.
///
/// For a date at or after the start it is that calendar year; before it, the previous one. This
/// is the resolver tests inject to prove `.FY` uses the fiscal calendar, not the year number.
#[derive(Clone, Copy, Debug)]
pub struct FiscalYearStart {
    /// The month the fiscal year starts in (`1..=12`).
    pub month: u8,
    /// The day of that month the fiscal year starts on.
    pub day: u8,
}

impl Default for FiscalYearStart {
    fn default() -> Self {
        Self { month: 1, day: 1 }
    }
}

impl FiscalYear for FiscalYearStart {
    fn fiscal_year(&self, date: Date) -> i32 {
        let (m, d) = (u8::from(date.month()), date.day());
        if (m, d) >= (self.month, self.day) {
            date.year()
        } else {
            date.year() - 1
        }
    }
}
