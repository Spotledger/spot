//! The language's values, and its own date arithmetic (no calendar library: the rules are
//! small and must be identical in every implementation).

use std::fmt;

use rust_decimal::Decimal;

/// The first day: 0001-01-01, as days since 1970-01-01.
const MIN_DAYS: i32 = -719_162;
/// The last day: 9999-12-31.
const MAX_DAYS: i32 = 2_932_896;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A calendar date, years 0001 to 9999, as days since 1970-01-01.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(i32);

impl Date {
    /// The date `days` after 1970-01-01, or `None` outside year 0001 to 9999.
    pub fn from_days(days: i32) -> Option<Date> {
        (MIN_DAYS..=MAX_DAYS).contains(&days).then_some(Date(days))
    }

    /// Days since 1970-01-01.
    pub fn days(self) -> i32 {
        self.0
    }

    /// A date from year, month (1 to 12) and day, or `None` if there is no such day.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Date> {
        if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
            return None;
        }
        if day > days_in_month(year, month) {
            return None;
        }
        Date::from_days(days_from_civil(year, month, day))
    }

    /// Year, month and day.
    pub fn ymd(self) -> (i32, u32, u32) {
        civil_from_days(self.0)
    }

    /// Parses `YYYY-MM-DD` exactly.
    pub fn parse(text: &str) -> Option<Date> {
        let b = text.as_bytes();
        if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
            return None;
        }
        let year = digits(b.get(0..4)?)?;
        let month = digits(b.get(5..7)?)?;
        let day = digits(b.get(8..10)?)?;
        Date::from_ymd(i32::try_from(year).ok()?, month, day)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// An instant in UTC with microsecond precision, years 0001 to 9999.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(i64);

impl DateTime {
    /// The instant `micros` after 1970-01-01T00:00:00Z, or `None` outside year 0001 to 9999.
    pub fn from_micros(micros: i64) -> Option<DateTime> {
        let lo = i64::from(MIN_DAYS) * MICROS_PER_DAY;
        let hi = (i64::from(MAX_DAYS) + 1) * MICROS_PER_DAY - 1;
        (lo..=hi).contains(&micros).then_some(DateTime(micros))
    }

    /// Microseconds since 1970-01-01T00:00:00Z.
    pub fn micros(self) -> i64 {
        self.0
    }

    /// The UTC date of the instant.
    pub fn date(self) -> Date {
        // The range of a DateTime is the range of a Date, so this cannot fail.
        let days = self.0.div_euclid(MICROS_PER_DAY);
        Date(i32::try_from(days).unwrap_or(0))
    }

    /// Midnight UTC of a date.
    pub fn from_date(date: Date) -> DateTime {
        DateTime(i64::from(date.0) * MICROS_PER_DAY)
    }

    /// Parses `YYYY-MM-DDTHH:MM:SS[.f{1,6}]Z` exactly.
    pub fn parse(text: &str) -> Option<DateTime> {
        let b = text.as_bytes();
        if b.len() < 20 || b.get(10) != Some(&b'T') || b.last() != Some(&b'Z') {
            return None;
        }
        let date = Date::parse(text.get(0..10)?)?;
        if b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
            return None;
        }
        let hour = digits(b.get(11..13)?)?;
        let minute = digits(b.get(14..16)?)?;
        let second = digits(b.get(17..19)?)?;
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let rest = b.get(19..b.len() - 1)?;
        let micros = if rest.is_empty() {
            0
        } else {
            let frac = rest.strip_prefix(b".")?;
            if frac.is_empty() || frac.len() > 6 {
                return None;
            }
            let value = digits(frac)?;
            // Scale to six digits: ".5" is 500000 microseconds.
            value.checked_mul(10_u32.pow(u32::try_from(6 - frac.len()).ok()?))?
        };
        let seconds = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
        DateTime::from_micros(
            i64::from(date.0) * MICROS_PER_DAY + seconds * 1_000_000 + i64::from(micros),
        )
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let day_micros = self.0.rem_euclid(MICROS_PER_DAY);
        let secs = day_micros / 1_000_000;
        let micros = day_micros % 1_000_000;
        write!(
            f,
            "{}T{:02}:{:02}:{:02}",
            self.date(),
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        )?;
        if micros != 0 {
            write!(f, ".{micros:06}")?;
        }
        f.write_str("Z")
    }
}

/// ASCII digits only, as a number.
fn digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0_u32, |acc, b| {
        acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))
    })
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i32::try_from(month).unwrap_or(1);
    let d = i32::try_from(day).unwrap_or(1);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Year, month and day of days since 1970-01-01.
fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i32::from(m <= 2);
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// A value of the language.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// No value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// An exact decimal. `PartialEq` compares the numeric value; the scale is compared by
    /// [`Value::same`].
    Decimal(Decimal),
    /// Text.
    Text(String),
    /// A calendar date.
    Date(Date),
    /// An instant.
    DateTime(DateTime),
    /// A list.
    List(Vec<Value>),
}

impl Value {
    /// Identity of type, value and (for decimals) scale: the comparison the corpus uses.
    pub fn same(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Decimal(a), Value::Decimal(b)) => a == b && a.scale() == b.scale(),
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y))
            }
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b) && a == b,
        }
    }

    /// The type's name, for messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Decimal(_) => "decimal",
            Value::Text(_) => "text",
            Value::Date(_) => "date",
            Value::DateTime(_) => "datetime",
            Value::List(_) => "list",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_ends_are_the_first_and_last_day() {
        assert_eq!(Date::from_days(MIN_DAYS).map(|d| d.ymd()), Some((1, 1, 1)));
        assert_eq!(
            Date::from_days(MAX_DAYS).map(|d| d.ymd()),
            Some((9999, 12, 31))
        );
        assert_eq!(Date::from_days(MIN_DAYS - 1), None);
        assert_eq!(Date::from_days(MAX_DAYS + 1), None);
        assert_eq!(Date::from_ymd(1970, 1, 1).map(Date::days), Some(0));
    }

    #[test]
    fn civil_conversion_round_trips_over_the_whole_range() {
        for days in (MIN_DAYS..=MAX_DAYS).step_by(97) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn leap_days() {
        assert!(Date::from_ymd(2024, 2, 29).is_some());
        assert!(Date::from_ymd(2023, 2, 29).is_none());
        assert!(Date::from_ymd(1900, 2, 29).is_none());
        assert!(Date::from_ymd(2000, 2, 29).is_some());
    }

    #[test]
    fn datetime_text_round_trips() {
        for text in [
            "2026-10-05T10:20:30Z",
            "0001-01-01T00:00:00Z",
            "9999-12-31T23:59:59.999999Z",
            "2026-10-05T10:20:30.500000Z",
        ] {
            assert_eq!(
                DateTime::parse(text).map(|d| d.to_string()).as_deref(),
                Some(text)
            );
        }
        assert_eq!(
            DateTime::parse("2026-10-05T10:20:30.5Z")
                .map(|d| d.to_string())
                .as_deref(),
            Some("2026-10-05T10:20:30.500000Z")
        );
        assert!(DateTime::parse("2026-10-05T10:20:30+00:00").is_none());
        assert!(DateTime::parse("2026-10-05T24:00:00Z").is_none());
        assert!(DateTime::parse("2026-10-05T10:20:30.1234567Z").is_none());
    }
}
