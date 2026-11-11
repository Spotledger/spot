//! Locale-tolerant parsing of certificate validity timestamps, the way `certutil -dump`
//! prints them.
//!
//! `certutil`'s date formatting comes from the machine's locale (the Win32
//! `GetDateFormatEx`/`GetTimeFormatEx` calls), so a single hard-coded format breaks on any
//! machine whose locale differs from the one that wrote a hard-coded pattern. This module
//! tries a short, **documented** list of formats and refuses anything else — an unparsed
//! date must never be silently treated as "valid forever" or "expired", either of which
//! would let [`crate::certificates::CertificateStore::bind_https`] make the wrong call.
//!
//! ## Supported formats (first match wins, tried in this order)
//!
//! 1. **ISO 8601 / RFC 3339** — the preferred, locale-independent source: PowerShell's
//!    `Get-ChildItem Cert:\...\| Select NotBefore,NotAfter` formatted with `.ToString("o")`
//!    (`2024-10-17T00:00:00.0000000Z`), or a plain `2024-10-17T00:00:00Z` /
//!    `2024-10-17T00:00:00+05:00`.
//! 2. **US English (`en-US`, `en-CA`, ...)** — month-first, with an AM/PM marker:
//!    `M/d/yyyy h:mm[:ss] AM|PM`, e.g. `10/17/2024 12:00:00 AM`.
//! 3. **Day-first, slash-separated, 24-hour** (`en-GB`, `fr-FR`, `es-ES`, most of Europe):
//!    `d/M/yyyy H:mm[:ss]`, e.g. `17/10/2024 00:00:00`.
//! 4. **Day-first, dot-separated, 24-hour** (`de-DE`, `de-AT`, `de-CH`):
//!    `d.M.yyyy H:mm[:ss]`, e.g. `17.10.2024 00:00:00`.
//!
//! Formats 2 and 3 share the `/` separator and the same field order is ambiguous between
//! them (`3/4/2024` could be March 4 or April 3). The AM/PM marker is the discriminator:
//! `certutil` always prints one for the US-English 12-hour clock and never for the
//! 24-hour locales above, so a trailing `AM`/`PM` selects format 2 and its absence selects
//! format 3. Any other shape — a locale this list does not cover, a named month, a
//! two-digit year, a missing field — is an error, not a guess.

use time::{Date, Month, PrimitiveDateTime, Time};

use crate::error::{WindowsSysError, WindowsSysResult};

/// Parses one `certutil`-style validity timestamp into whole seconds since the Unix epoch
/// (UTC). See the module docs for the exact, ordered list of formats tried.
///
/// # Errors
/// [`WindowsSysError::io`] when `text` matches none of the supported formats.
pub fn parse_certificate_datetime(text: &str) -> WindowsSysResult<i64> {
    let trimmed = text.trim();
    parse_iso8601(trimmed)
        .or_else(|| parse_us_en(trimmed))
        .or_else(|| parse_day_first(trimmed, '/'))
        .or_else(|| parse_day_first(trimmed, '.'))
        .ok_or_else(|| {
            WindowsSysError::io(format!(
                "the certificate validity timestamp '{trimmed}' does not match any \
                 supported locale format (iso-8601, en-US, day-first slash, day-first dot)"
            ))
        })
}

/// Finds the `NotBefore:`/`NotAfter:` lines in a `certutil -dump` (or `-store`) listing and
/// parses both into whole seconds since the Unix epoch (UTC). Pure and platform-independent:
/// it never runs a tool, so it is exercised with recorded fixtures on Linux.
///
/// # Errors
/// [`WindowsSysError::io`] when either line is missing or does not parse.
pub fn parse_certutil_dump_validity(dump: &str) -> WindowsSysResult<(i64, i64)> {
    let not_before = value_after(dump, "NotBefore:")
        .ok_or_else(|| WindowsSysError::io("certutil output has no NotBefore line"))?;
    let not_after = value_after(dump, "NotAfter:")
        .ok_or_else(|| WindowsSysError::io("certutil output has no NotAfter line"))?;
    Ok((
        parse_certificate_datetime(&not_before)?,
        parse_certificate_datetime(&not_after)?,
    ))
}

/// A line's value after `key`, trimmed; `None` when the key is absent. Matches the first
/// occurrence, same as [`crate::windows`]'s own `value_after`.
fn value_after(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(key))
        .map(|rest| rest.trim().to_owned())
}

fn to_unix(date: Date, time_of_day: Time) -> Option<i64> {
    Some(
        PrimitiveDateTime::new(date, time_of_day)
            .assume_utc()
            .unix_timestamp(),
    )
}

fn build_date(year: i32, month: u8, day: u8) -> Option<Date> {
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

fn parse_time_fields(text: &str) -> Option<(u8, u8, u8)> {
    let mut fields = text.split(':');
    let hour: u8 = fields.next()?.trim().parse().ok()?;
    let minute: u8 = fields.next()?.trim().parse().ok()?;
    let second: u8 = match fields.next() {
        Some(value) => value.trim().parse().ok()?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    Some((hour, minute, second))
}

/// `2024-10-17T00:00:00[.fraction](Z|+HH:MM|-HH:MM)`.
fn parse_iso8601(text: &str) -> Option<i64> {
    if text.len() < 20 {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i32 = text.get(0..4)?.parse().ok()?;
    let month: u8 = text.get(5..7)?.parse().ok()?;
    let day: u8 = text.get(8..10)?.parse().ok()?;
    let hour: u8 = text.get(11..13)?.parse().ok()?;
    let minute: u8 = text.get(14..16)?.parse().ok()?;
    let second: u8 = text.get(17..19)?.parse().ok()?;
    let offset_seconds = parse_iso_offset(&text[19..])?;
    let date = build_date(year, month, day)?;
    let time_of_day = Time::from_hms(hour, minute, second).ok()?;
    let unix = to_unix(date, time_of_day)?;
    Some(unix - i64::from(offset_seconds))
}

/// `.fraction` (optional) then `Z` or `+HH:MM`/`-HH:MM`. Returns the offset in seconds,
/// positive east of UTC (so UTC = local - offset).
fn parse_iso_offset(rest: &str) -> Option<i32> {
    let marker_start = rest.find(['Z', '+', '-'])?;
    let marker = &rest[marker_start..];
    if marker == "Z" {
        return Some(0);
    }
    let sign: i32 = if marker.starts_with('-') { -1 } else { 1 };
    let digits = &marker[1..];
    if digits.len() != 5 || digits.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let hours: i32 = digits.get(0..2)?.parse().ok()?;
    let minutes: i32 = digits.get(3..5)?.parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// `M/d/yyyy h:mm[:ss] AM|PM`, the US-English 12-hour clock.
fn parse_us_en(text: &str) -> Option<i64> {
    let upper = text.to_ascii_uppercase();
    let is_pm = if upper.ends_with("AM") {
        false
    } else if upper.ends_with("PM") {
        true
    } else {
        return None;
    };
    let body = text[..text.len() - 2].trim_end();
    let mut parts = body.split_whitespace();
    let date_str = parts.next()?;
    let time_str = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut fields = date_str.split('/');
    let month: u8 = fields.next()?.parse().ok()?;
    let day: u8 = fields.next()?.parse().ok()?;
    let year: i32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let (hour12, minute, second) = parse_time_fields(time_str)?;
    if !(1..=12).contains(&hour12) {
        return None;
    }
    let hour = match (hour12, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (value, false) => value,
        (value, true) => value + 12,
    };
    let date = build_date(year, month, day)?;
    let time_of_day = Time::from_hms(hour, minute, second).ok()?;
    to_unix(date, time_of_day)
}

/// `d<sep>M<sep>yyyy H:mm[:ss]`, a day-first 24-hour clock with the given date separator.
fn parse_day_first(text: &str, sep: char) -> Option<i64> {
    let mut parts = text.split_whitespace();
    let date_str = parts.next()?;
    let time_str = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    // AM/PM text in this slot means it was the US format above, not this one.
    if time_str.to_ascii_uppercase() != time_str && time_str.chars().any(char::is_alphabetic) {
        return None;
    }
    let mut fields = date_str.split(sep);
    let day: u8 = fields.next()?.parse().ok()?;
    let month: u8 = fields.next()?.parse().ok()?;
    let year: i32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let (hour, minute, second) = parse_time_fields(time_str)?;
    let date = build_date(year, month, day)?;
    let time_of_day = Time::from_hms(hour, minute, second).ok()?;
    to_unix(date, time_of_day)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_parses_with_and_without_fraction() {
        assert_eq!(
            parse_certificate_datetime("2024-10-17T00:00:00Z").unwrap(),
            1_729_123_200
        );
        assert_eq!(
            parse_certificate_datetime("2024-10-17T00:00:00.0000000Z").unwrap(),
            1_729_123_200
        );
    }

    #[test]
    fn iso8601_honours_a_non_zero_offset() {
        // 05:00 local is midnight UTC when the offset is +05:00.
        assert_eq!(
            parse_certificate_datetime("2024-10-17T05:00:00+05:00").unwrap(),
            1_729_123_200
        );
    }

    #[test]
    fn us_en_parses_month_first_with_am_pm() {
        assert_eq!(
            parse_certificate_datetime("10/17/2024 12:00:00 AM").unwrap(),
            1_729_123_200
        );
        assert_eq!(
            parse_certificate_datetime("10/17/2024 12:00 AM").unwrap(),
            1_729_123_200
        );
    }

    #[test]
    fn day_first_slash_parses_the_same_instant_differently_ordered() {
        assert_eq!(
            parse_certificate_datetime("17/10/2024 00:00:00").unwrap(),
            1_729_123_200
        );
    }

    #[test]
    fn day_first_dot_parses_the_german_style() {
        assert_eq!(
            parse_certificate_datetime("17.10.2024 00:00:00").unwrap(),
            1_729_123_200
        );
    }

    #[test]
    fn an_unsupported_format_is_an_error_not_a_pass() {
        assert!(parse_certificate_datetime("17 Oct 2024 00:00:00").is_err());
        assert!(parse_certificate_datetime("not a date").is_err());
        assert!(parse_certificate_datetime("").is_err());
    }

    #[test]
    fn dump_validity_reads_both_lines() {
        let dump = "Subject: CN=spot\nNotBefore: 10/17/2024 12:00:00 AM\nNotAfter: 10/17/2025 11:59:59 PM\n";
        let (before, after) = parse_certutil_dump_validity(dump).unwrap();
        assert!(before < after);
    }

    #[test]
    fn dump_validity_is_an_error_when_a_line_is_missing() {
        assert!(parse_certutil_dump_validity("Subject: CN=spot\n").is_err());
        assert!(parse_certutil_dump_validity("NotBefore: 10/17/2024 12:00:00 AM\n").is_err());
    }
}
