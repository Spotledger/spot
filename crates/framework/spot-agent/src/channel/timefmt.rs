//! Calendar arithmetic without a clock: RFC 3339 formatting of Unix seconds and parsing
//! of the HTTP `Date` header (IMF-fixdate), used for liveness stamps and skew learning.

/// Days since 1970-01-01 of the civil date (proleptic Gregorian).
#[must_use]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01.
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds.
#[must_use]
pub fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Parses `Sun, 06 Nov 1994 08:49:37 GMT` into Unix seconds.
#[must_use]
pub fn parse_http_date(text: &str) -> Option<i64> {
    let rest = text.split_once(", ")?.1;
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|m| *m == month_name)?).ok()? + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let s: i64 = hms.next()?.parse().ok()?;
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + mi * 60 + s)
}
