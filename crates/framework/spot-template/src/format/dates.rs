//! The `date` and `datetime` renderers.

use minijinja::Value;
use time::{Date, OffsetDateTime, UtcOffset, Weekday};

use crate::error::TemplateError;
use crate::value::{DateObject, DateTimeObject};

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const WEEKDAYS_EN: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// Formats a ctx date value. Formats: `iso` (2025-01-15), `short`
/// (15/01/2025), `medium` (Jan 15, 2025), `long`
/// (15 January 2025 (Wednesday)). The English names apply in every locale of
/// this task's set; unknown names are errors, never a silent default.
pub(crate) fn date(value: &Value, format: &str) -> Result<String, TemplateError> {
    let date = match value.downcast_object_ref::<DateObject>() {
        Some(object) => object.date(),
        None => {
            return Err(TemplateError::InvalidInput {
                message: "date expects a ctx date (CtxValue::Date); strings from the \
                     template are not dates — pass dates in the context and \
                     format them, never parse them"
                    .to_owned(),
            });
        }
    };
    date_part(date, format)
}

/// Formats a ctx datetime value, optionally shifted to `tz`
/// (`UTC`, `+HH:MM`, `-HH:MM`). Uses the same format names as [`date`];
/// datetimes add the time (HH:MM, or HH:MM:SS for `long`), and `iso` prints
/// the full timestamp with its offset.
pub(crate) fn datetime(
    value: &Value,
    format: &str,
    tz: Option<&str>,
) -> Result<String, TemplateError> {
    let datetime = match value.downcast_object_ref::<DateTimeObject>() {
        Some(object) => object.datetime(),
        None => {
            return Err(TemplateError::InvalidInput {
                message: "datetime expects a ctx datetime (CtxValue::DateTime); strings \
                     from the template are not datetimes — pass datetimes in \
                     the context"
                    .to_owned(),
            });
        }
    };
    let datetime = shift(datetime, tz)?;
    match format {
        "iso" => Ok(datetime.to_string()),
        "short" | "medium" => Ok(format!(
            "{} {:02}:{:02}",
            date_part(datetime.date(), format)?,
            datetime.hour(),
            datetime.minute()
        )),
        "long" => Ok(format!(
            "{} {:02}:{:02}:{:02}",
            date_part(datetime.date(), format)?,
            datetime.hour(),
            datetime.minute(),
            datetime.second()
        )),
        other => Err(TemplateError::InvalidDateFormat {
            format: other.to_owned(),
        }),
    }
}

fn date_part(date: Date, format: &str) -> Result<String, TemplateError> {
    let (year, month, day) = date.to_calendar_date();
    let month_number = u8::from(month);
    let month_full = MONTHS_FULL[usize::from(month_number - 1)];
    let month_short = MONTHS_SHORT[usize::from(month_number - 1)];
    match format {
        "iso" => Ok(date.to_string()),
        "short" => Ok(format!("{day:02}/{month_number:02}/{year}")),
        "medium" => Ok(format!("{month_short} {day:02}, {year}")),
        "long" => Ok(format!(
            "{day} {month_full} {year} ({})",
            WEEKDAYS_EN[weekday_index(date.weekday())]
        )),
        other => Err(TemplateError::InvalidDateFormat {
            format: other.to_owned(),
        }),
    }
}

fn weekday_index(weekday: Weekday) -> usize {
    match weekday {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

fn shift(datetime: OffsetDateTime, tz: Option<&str>) -> Result<OffsetDateTime, TemplateError> {
    match tz {
        None => Ok(datetime),
        Some(name) => {
            let offset = parse_offset(name)?;
            match datetime.checked_to_offset(offset) {
                Some(shifted) => Ok(shifted),
                None => Err(TemplateError::InvalidInput {
                    message: format!("datetime '{datetime}' cannot be shifted to '{name}'"),
                }),
            }
        }
    }
}

fn parse_offset(name: &str) -> Result<UtcOffset, TemplateError> {
    if name.eq_ignore_ascii_case("UTC") {
        return Ok(UtcOffset::UTC);
    }
    let (negative, rest) = match name.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, name.strip_prefix('+').unwrap_or(name)),
    };
    let not_an_offset = || TemplateError::InvalidInput {
        message: format!("tz '{name}' is not an offset like '+05:00', '-03:30' or 'UTC'"),
    };
    let (hours_text, minutes_text) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None => (rest, "0"),
    };
    let hours = hours_text.parse::<i16>().map_err(|_| not_an_offset())?;
    let minutes = minutes_text.parse::<i16>().map_err(|_| not_an_offset())?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return Err(TemplateError::InvalidInput {
            message: format!("tz '{name}' is out of range (offsets are ±23:59)"),
        });
    }
    let signed_hours = if negative { -hours } else { hours };
    let signed_minutes = if negative { -minutes } else { minutes };
    let hours_i8 = i8::try_from(signed_hours).map_err(|_| not_an_offset())?;
    let minutes_i8 = i8::try_from(signed_minutes).map_err(|_| not_an_offset())?;
    UtcOffset::from_hms(hours_i8, minutes_i8, 0).map_err(|_| TemplateError::InvalidInput {
        message: format!("tz '{name}' is out of range (offsets are ±23:59)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::DecimalObject;
    use rust_decimal::Decimal;

    fn jan15() -> Date {
        match Date::from_calendar_date(2025, time::Month::January, 15) {
            Ok(date) => date,
            Err(error) => panic!("bad date: {error}"),
        }
    }

    fn date_value(date: Date) -> Value {
        Value::from_object(DateObject::new(date))
    }

    fn datetime_value(datetime: OffsetDateTime) -> Value {
        Value::from_object(DateTimeObject::new(datetime))
    }

    fn at930() -> OffsetDateTime {
        jan15()
            .with_hms(9, 30, 0)
            .unwrap_or_else(|e| panic!("{e}"))
            .assume_utc()
    }

    #[test]
    fn date_formats() {
        let value = date_value(jan15()); // a Wednesday
        assert_eq!(
            date(&value, "iso").unwrap_or_else(|e| panic!("{e}")),
            "2025-01-15"
        );
        assert_eq!(
            date(&value, "short").unwrap_or_else(|e| panic!("{e}")),
            "15/01/2025"
        );
        assert_eq!(
            date(&value, "medium").unwrap_or_else(|e| panic!("{e}")),
            "Jan 15, 2025"
        );
        assert_eq!(
            date(&value, "long").unwrap_or_else(|e| panic!("{e}")),
            "15 January 2025 (Wednesday)"
        );
    }

    #[test]
    fn non_dates_are_rejected() {
        let string_value = Value::from("2025-01-15");
        match date(&string_value, "iso") {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("ctx date"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        let decimal_value = Value::from_object(DecimalObject::new(Decimal::ONE));
        match datetime(&decimal_value, "iso", None) {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("ctx datetime"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn unknown_format_is_an_error() {
        match date(&date_value(jan15()), "fancy") {
            Err(TemplateError::InvalidDateFormat { format }) => assert_eq!(format, "fancy"),
            other => panic!("expected InvalidDateFormat, got {other:?}"),
        }
        match datetime(&datetime_value(at930()), "fancy", None) {
            Err(TemplateError::InvalidDateFormat { .. }) => {}
            other => panic!("expected InvalidDateFormat, got {other:?}"),
        }
    }

    #[test]
    fn tz_shifts_are_exact() {
        let value = datetime_value(at930());
        assert_eq!(
            datetime(&value, "medium", Some("+05:00")).unwrap_or_else(|e| panic!("{e}")),
            "Jan 15, 2025 14:30"
        );
        assert_eq!(
            datetime(&value, "medium", Some("-05:30")).unwrap_or_else(|e| panic!("{e}")),
            "Jan 15, 2025 04:00"
        );
        assert_eq!(
            datetime(&value, "medium", Some("UTC")).unwrap_or_else(|e| panic!("{e}")),
            "Jan 15, 2025 09:30"
        );
        assert_eq!(
            datetime(&value, "long", Some("+00:00")).unwrap_or_else(|e| panic!("{e}")),
            "15 January 2025 (Wednesday) 09:30:00"
        );
        let iso = datetime(&value, "iso", Some("+05:00")).unwrap_or_else(|e| panic!("{e}"));
        assert!(iso.contains("14:30") && iso.contains("+05"), "{iso}");
    }

    #[test]
    fn bad_tz_names_are_errors() {
        let value = datetime_value(at930());
        for bad in ["bogus", "+99:00", "+05:60"] {
            match datetime(&value, "medium", Some(bad)) {
                Err(TemplateError::InvalidInput { .. }) => {}
                other => panic!("'{bad}' expected InvalidInput, got {other:?}"),
            }
        }
    }
}
