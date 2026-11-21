//! Strict numbers and calendar fields, no float and no locale.

use rust_decimal::Decimal;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

/// Sign, digits, and at most one dot with digits on both sides. No exponent, no separators,
/// no spaces.
pub(crate) fn number_shape(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    let mut parts = body.split('.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    parts.next().is_none()
        && !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) fn decimal(text: &str) -> Option<Decimal> {
    if !number_shape(text) {
        return None;
    }
    Decimal::from_str_exact(text).ok()
}

pub(crate) fn int(text: &str) -> Option<i64> {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>().ok()
}

fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok()
}

/// `YYYY-MM-DD`.
pub(crate) fn date(text: &str) -> Option<Date> {
    let b = text.as_bytes();
    if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let year = i32::try_from(digits(text.get(0..4)?)?).ok()?;
    let month = Month::try_from(u8::try_from(digits(text.get(5..7)?)?).ok()?).ok()?;
    let day = u8::try_from(digits(text.get(8..10)?)?).ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

/// `YYYY-MM-DDTHH:MM:SS`, an optional fraction of 1 to 9 digits, then `Z` (or `+00:00`).
pub(crate) fn instant(text: &str) -> Option<OffsetDateTime> {
    let body = text
        .strip_suffix('Z')
        .or_else(|| text.strip_suffix("+00:00"))?;
    let (head, frac) = match body.split_once('.') {
        Some((head, frac)) => (head, Some(frac)),
        None => (body, None),
    };
    let (day, clock) = head.split_once('T')?;
    let date = date(day)?;
    let c = clock.as_bytes();
    if c.len() != 8 || c.get(2) != Some(&b':') || c.get(5) != Some(&b':') {
        return None;
    }
    let hour = u8::try_from(digits(clock.get(0..2)?)?).ok()?;
    let minute = u8::try_from(digits(clock.get(3..5)?)?).ok()?;
    let second = u8::try_from(digits(clock.get(6..8)?)?).ok()?;
    let nanos = match frac {
        None => 0,
        Some(f) if (1..=9).contains(&f.len()) => {
            let n = digits(f)?;
            // Scale the fraction to nine digits: 5 digits is n * 10^4.
            n.checked_mul(10u32.pow(9 - f.len() as u32))?
        }
        Some(_) => return None,
    };
    let time = Time::from_hms_nano(hour, minute, second, nanos).ok()?;
    Some(PrimitiveDateTime::new(date, time).assume_utc())
}
