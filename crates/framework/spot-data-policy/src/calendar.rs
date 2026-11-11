//! Calendar-aware periods and the injected [`Calendar`].
//!
//! Retention is expressed in days, months or years, never in "seconds since": a rule of seven
//! years means the same calendar day seven years later, and 31 January plus one month is the last
//! day of February, not 3 March. The arithmetic is integer-only (ADR-12) and works on a
//! [`CivilDate`] — a proleptic Gregorian date in UTC. Nothing here reads a clock; the caller
//! supplies `now` as Unix milliseconds.
//!
//! A host whose legal calendar is not Gregorian (a fiscal calendar, for example) injects its own
//! [`Calendar`] implementation; the rule, the planner and the executor all take `&dyn Calendar`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::DataPolicyError;

/// Milliseconds in one day. Retention dates are whole UTC days; `now` may fall anywhere in one.
pub const MILLIS_PER_DAY: u64 = 86_400_000;

/// A proleptic Gregorian calendar date in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CivilDate {
    /// The year (the platform supports years 0 to 9999).
    pub year: i32,
    /// The month, 1 to 12.
    pub month: u8,
    /// The day, 1 to the month's last day.
    pub day: u8,
}

impl CivilDate {
    /// Builds a date and checks it exists in the Gregorian calendar.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the month or day is out of range.
    pub fn new(year: i32, month: u8, day: u8) -> Result<Self, DataPolicyError> {
        if !(0..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
            return Err(DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a date outside year 0-9999, month 1-12 or day 1-31".to_owned(),
            });
        }
        if day > days_in_month(year, month) {
            return Err(DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a day past the end of its month".to_owned(),
            });
        }
        Ok(Self { year, month, day })
    }

    /// The UTC date containing `millis` since the Unix epoch.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the instant is beyond year 9999.
    pub fn from_unix_millis(millis: u64) -> Result<Self, DataPolicyError> {
        let days = i64::try_from(millis / MILLIS_PER_DAY).map_err(|_| {
            DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "an instant beyond the supported year range".to_owned(),
            }
        })?;
        let (year, month, day) = civil_from_days(days);
        Self::new(
            i32::try_from(year).map_err(|_| DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a year outside the supported range".to_owned(),
            })?,
            month,
            day,
        )
    }

    /// Midnight UTC at the start of this date, in milliseconds since the Unix epoch.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the date precedes the epoch (the platform does
    /// not store pre-1970 retention dates).
    pub fn to_unix_millis(self) -> Result<u64, DataPolicyError> {
        let days = days_from_civil(
            i64::from(self.year),
            u32::from(self.month),
            u32::from(self.day),
        );
        if days < 0 {
            return Err(DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a date before 1970".to_owned(),
            });
        }
        u64::try_from(days)
            .ok()
            .and_then(|d| d.checked_mul(MILLIS_PER_DAY))
            .ok_or_else(|| DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a date beyond the supported instant range".to_owned(),
            })
    }

    /// This date `count` days later (negative counts move backwards).
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the result leaves the supported range.
    pub fn add_days(self, count: i64) -> Result<Self, DataPolicyError> {
        let days = days_from_civil(
            i64::from(self.year),
            u32::from(self.month),
            u32::from(self.day),
        );
        let moved = days
            .checked_add(count)
            .ok_or_else(|| DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a date beyond the supported range".to_owned(),
            })?;
        let (year, month, day) = civil_from_days(moved);
        Self::new(
            i32::try_from(year).map_err(|_| DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a year outside the supported range".to_owned(),
            })?,
            month,
            day,
        )
    }

    /// This date `months` later, clamping to the last day of the target month (31 January plus
    /// one month is 28 or 29 February).
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the result leaves the supported range.
    pub fn add_months(self, months: u64) -> Result<Self, DataPolicyError> {
        let months = i64::try_from(months).map_err(|_| DataPolicyError::InvalidCalendar {
            calendar: "gregorian".to_owned(),
            detail: "a period beyond the supported range".to_owned(),
        })?;
        let total = i64::from(self.year)
            .checked_mul(12)
            .and_then(|v| v.checked_add(i64::from(self.month) - 1))
            .and_then(|v| v.checked_add(months))
            .ok_or_else(|| DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a period beyond the supported range".to_owned(),
            })?;
        let year = total.div_euclid(12);
        let month = u8::try_from(total.rem_euclid(12) + 1).map_err(|_| {
            DataPolicyError::InvalidCalendar {
                calendar: "gregorian".to_owned(),
                detail: "a month outside the supported range".to_owned(),
            }
        })?;
        let year = i32::try_from(year).map_err(|_| DataPolicyError::InvalidCalendar {
            calendar: "gregorian".to_owned(),
            detail: "a year outside the supported range".to_owned(),
        })?;
        let day = self.day.min(days_in_month(year, month));
        Self::new(year, month, day)
    }
}

impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// The number of days in a Gregorian month.
#[must_use]
pub fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Whether `year` is a Gregorian leap year.
#[must_use]
pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm; integer only).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month_prime = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date `days` after 1970-01-01 (Howard Hinnant's algorithm; integer only).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u8, day as u8)
}

/// The unit of a retention period.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodUnit {
    /// Whole calendar days.
    Days,
    /// Calendar months, clamped at the end of the target month.
    Months,
    /// Calendar years (twelve calendar months).
    Years,
}

impl PeriodUnit {
    /// The unit suffix used in metadata text.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Days => "d",
            Self::Months => "m",
            Self::Years => "y",
        }
    }
}

/// A calendar-aware retention period, written in metadata as `90d`, `18m` or `7y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Period {
    /// How many units.
    pub count: u32,
    /// Which unit.
    pub unit: PeriodUnit,
}

impl Period {
    /// A period of `count` days.
    #[must_use]
    pub const fn days(count: u32) -> Self {
        Self {
            count,
            unit: PeriodUnit::Days,
        }
    }

    /// A period of `count` months.
    #[must_use]
    pub const fn months(count: u32) -> Self {
        Self {
            count,
            unit: PeriodUnit::Months,
        }
    }

    /// A period of `count` years.
    #[must_use]
    pub const fn years(count: u32) -> Self {
        Self {
            count,
            unit: PeriodUnit::Years,
        }
    }

    /// The period in the metadata text form.
    #[must_use]
    pub fn as_text(self) -> String {
        format!("{}{}", self.count, self.unit.suffix())
    }
}

impl FromStr for Period {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        let (digits, unit) = text.split_at(text.len().saturating_sub(1));
        let count = digits
            .parse::<u32>()
            .map_err(|_| "a period is a number followed by d, m or y".to_owned())?;
        let unit = match unit {
            "d" => PeriodUnit::Days,
            "m" => PeriodUnit::Months,
            "y" => PeriodUnit::Years,
            _ => return Err("a period is a number followed by d, m or y".to_owned()),
        };
        Ok(Self { count, unit })
    }
}

impl fmt::Display for Period {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_text())
    }
}

/// The calendar retention arithmetic runs on.
///
/// The reference implementation is [`Gregorian`]. A host with a different legal calendar injects
/// its own; the planner and executor never assume Gregorian arithmetic themselves.
pub trait Calendar: Send + Sync {
    /// The calendar's stable name, written into plans and execution checkpoints.
    fn name(&self) -> &'static str;

    /// Adds `period` to `date`, clamping at the end of a month.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the result leaves the calendar's range.
    fn add(&self, date: CivilDate, period: Period) -> Result<CivilDate, DataPolicyError>;

    /// The date `period` after the instant `millis`, in Unix milliseconds at midnight UTC of the
    /// resulting day.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidCalendar`] when the instant or the result is out of range.
    fn due_at(&self, millis: u64, period: Period) -> Result<u64, DataPolicyError> {
        self.add(CivilDate::from_unix_millis(millis)?, period)?
            .to_unix_millis()
    }
}

/// The proleptic Gregorian calendar in UTC.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gregorian;

impl Calendar for Gregorian {
    fn name(&self) -> &'static str {
        "gregorian"
    }

    fn add(&self, date: CivilDate, period: Period) -> Result<CivilDate, DataPolicyError> {
        match period.unit {
            PeriodUnit::Days => date.add_days(i64::from(period.count)),
            PeriodUnit::Months => date.add_months(u64::from(period.count)),
            PeriodUnit::Years => date.add_months(u64::from(period.count) * 12),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{
        CivilDate, Gregorian, MILLIS_PER_DAY, Period, PeriodUnit, days_in_month, is_leap_year,
    };
    use crate::calendar::Calendar;

    #[test]
    fn a_month_clamps_to_the_end_of_february() {
        let january = CivilDate::new(2024, 1, 31).unwrap();
        assert_eq!(
            january.add_months(1).unwrap(),
            CivilDate::new(2024, 2, 29).unwrap()
        );
        let january = CivilDate::new(2023, 1, 31).unwrap();
        assert_eq!(
            january.add_months(1).unwrap(),
            CivilDate::new(2023, 2, 28).unwrap()
        );
    }

    #[test]
    fn leap_years_follow_the_gregorian_rule() {
        assert!(is_leap_year(2000));
        assert!(!is_leap_year(1900));
        assert!(is_leap_year(2024));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
    }

    #[test]
    fn unix_millis_round_trip_at_day_boundaries() {
        let date = CivilDate::new(2026, 10, 19).unwrap();
        let millis = date.to_unix_millis().unwrap();
        assert_eq!(millis % MILLIS_PER_DAY, 0);
        assert_eq!(CivilDate::from_unix_millis(millis).unwrap(), date);
        assert_eq!(
            CivilDate::from_unix_millis(millis + MILLIS_PER_DAY - 1).unwrap(),
            date
        );
        assert_eq!(
            CivilDate::from_unix_millis(millis + MILLIS_PER_DAY).unwrap(),
            CivilDate::new(2026, 10, 20).unwrap()
        );
    }

    #[test]
    fn the_reference_calendar_adds_years_as_months() {
        let date = CivilDate::new(2020, 2, 29).unwrap();
        assert_eq!(
            Gregorian.add(date, Period::years(1)).unwrap(),
            CivilDate::new(2021, 2, 28).unwrap()
        );
        assert_eq!(
            Gregorian.add(date, Period::days(1)).unwrap(),
            CivilDate::new(2020, 3, 1).unwrap()
        );
    }

    #[test]
    fn periods_parse_and_print() {
        assert_eq!("90d".parse::<Period>().unwrap(), Period::days(90));
        assert_eq!("18m".parse::<Period>().unwrap(), Period::months(18));
        assert_eq!("7y".parse::<Period>().unwrap(), Period::years(7));
        assert_eq!("7y".parse::<Period>().unwrap().as_text(), "7y");
        assert_eq!(
            Period {
                count: 1,
                unit: PeriodUnit::Days
            }
            .to_string(),
            "1d"
        );
        assert!("7".parse::<Period>().is_err());
        assert!("7w".parse::<Period>().is_err());
        assert!("-1d".parse::<Period>().is_err());
    }
}
