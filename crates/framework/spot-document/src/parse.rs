//! Parsing of the default values metadata stores as strings.
//!
//! The formats are the ones `spot-meta` validates: `YYYY-MM-DD`, `HH:MM[:SS[.f]]` and
//! `YYYY-MM-DDTHH:MM[:SS[.f]]` followed by `Z` or `+HH:MM` / `-HH:MM`. Nothing here rounds or
//! guesses: text that is not in the format is `None`.

use spot_app_api::prelude::{Date, OffsetDateTime, Time};

fn digits(s: &str, len: usize) -> Option<u32> {
    if s.len() == len && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// `YYYY-MM-DD`.
pub(crate) fn date(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (year, month, day) = (digits(y, 4)?, digits(m, 2)?, digits(d, 2)?);
    let year = i32::try_from(year).ok()?;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let month_index = usize::try_from(month.checked_sub(1)?).ok()?;
    let length = *lengths.get(month_index)?;
    if day == 0 || day > length {
        return None;
    }
    let before: u32 = lengths.iter().take(month_index).sum();
    Date::from_ordinal_date(year, u16::try_from(before + day).ok()?).ok()
}

/// `HH:MM`, `HH:MM:SS` or `HH:MM:SS.f` (up to nine fraction digits).
pub(crate) fn time(s: &str) -> Option<Time> {
    let (main, fraction) = match s.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (s, None),
    };
    let parts: Vec<&str> = main.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m] if fraction.is_none() => (*h, *m, "00"),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    let nanos = match fraction {
        None => 0,
        Some(f) if (1..=9).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()) => {
            let value: u32 = f.parse().ok()?;
            value * 10_u32.pow(u32::try_from(9 - f.len()).ok()?)
        }
        Some(_) => return None,
    };
    Time::from_hms_nano(
        u8::try_from(digits(h, 2)?).ok()?,
        u8::try_from(digits(m, 2)?).ok()?,
        u8::try_from(digits(sec, 2)?).ok()?,
        nanos,
    )
    .ok()
}

/// `YYYY-MM-DDTHH:MM[:SS[.f]]` with `Z` or an offset, as the instant in UTC. Years 0001 to 9998
/// only, so that applying an offset can never leave the supported range.
pub(crate) fn datetime(s: &str) -> Option<OffsetDateTime> {
    let (date_part, rest) = s.split_once('T')?;
    let date = date(date_part)?;
    if !(1..=9998).contains(&date.year()) {
        return None;
    }
    let (time_part, offset_minutes) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0_i64)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (t, offset) = rest.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (h, m) = offset.get(1..)?.split_once(':')?;
        let minutes = i64::from(digits(h, 2)?) * 60 + i64::from(digits(m, 2)?);
        if digits(h, 2)? > 23 || digits(m, 2)? > 59 {
            return None;
        }
        (t, sign * minutes)
    };
    let local = date.with_time(time(time_part)?).assume_utc();
    // The wall-clock time is `offset` ahead of UTC, so UTC is `offset` earlier.
    let shift = std::time::Duration::from_secs(offset_minutes.unsigned_abs() * 60);
    Some(if offset_minutes >= 0 {
        local - shift
    } else {
        local + shift
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dates_follow_the_calendar() {
        assert_eq!(
            date("2026-09-26").map(|d| d.to_string()),
            Some("2026-09-26".into())
        );
        assert_eq!(
            date("2024-02-29").map(|d| d.to_string()),
            Some("2024-02-29".into())
        );
        for bad in [
            "2026-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "26-01-01",
            "2026-1-1",
            "2026-01-01-01",
            "",
        ] {
            assert!(date(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn times_accept_the_three_forms() {
        assert_eq!(time("09:30").unwrap().to_string(), "9:30:00.0");
        assert_eq!(time("09:30:15").unwrap().to_string(), "9:30:15.0");
        assert_eq!(time("09:30:15.5").unwrap().nanosecond(), 500_000_000);
        for bad in [
            "24:00",
            "09:60",
            "09:30:61",
            "9:30",
            "09:30.5",
            "09:30:15.",
            "09:30:15.1234567890",
            "09",
        ] {
            assert!(time(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn datetimes_become_utc_instants() {
        let z = datetime("2026-09-26T10:00:00Z").unwrap();
        let karachi = datetime("2026-09-26T15:00:00+05:00").unwrap();
        let west = datetime("2026-09-26T05:00:00-05:00").unwrap();
        assert_eq!(z, karachi);
        assert_eq!(z, west);
        for bad in [
            "2026-09-26",
            "2026-09-26T10:00:00",
            "2026-09-26T10:00:00+5:00",
            "2026-09-26T10:00:00+24:00",
            "0000-01-01T00:00:00Z",
            "9999-12-31T00:00:00Z",
        ] {
            assert!(datetime(bad).is_none(), "{bad}");
        }
    }
}
