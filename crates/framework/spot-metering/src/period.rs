//! Period buckets: which monthly window an instant belongs to, in the tenant's zone.
//!
//! ADR-077: instants are UTC seconds; periods are business dates in the business
//! zone. A period is a half-open window `[start, end_exclusive)`, and
//! [`period_of`] assigns every instant exactly one bucket — across DST changes and
//! month ends — because each period's `end_exclusive` is by construction the next
//! period's `start`, computed by the same code.
//!
//! Two bases:
//!
//! - [`PeriodBasis::CalendarMonth`]: the calendar month in the tenant's zone (the
//!   default);
//! - [`PeriodBasis::Anniversary`]: the licence-anniversary month — windows run from
//!   the anchor day-of-month to the next, clamped to months shorter than the anchor
//!   (an anchor of 31 lands on Feb 28, or Feb 29 in a leap year).
//!
//! No clock is read: the instant is always an argument.

use jiff::Timestamp;
use jiff::civil::Date;

use crate::error::MeteringError;

/// How a period window is anchored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeriodBasis {
    /// Calendar months in the tenant's zone.
    CalendarMonth,
    /// Licence-anniversary windows: from the anchor day-of-month (inclusive, local
    /// midnight) to the next anchor day. Clamped to each month's length.
    Anniversary {
        /// The day of month the window starts on, 1..=31.
        anchor_day: u8,
    },
}

impl PeriodBasis {
    /// The short label used in reports: `calendar` or `anniversary:<day>`.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::CalendarMonth => "calendar".to_owned(),
            Self::Anniversary { anchor_day } => format!("anniversary:{anchor_day}"),
        }
    }
}

/// A period specification: which zone buckets in, and how windows are anchored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeriodSpec {
    zone: String,
    basis: PeriodBasis,
}

impl PeriodSpec {
    /// Calendar months in `zone` (an IANA name such as `Asia/Karachi`).
    ///
    /// # Errors
    /// [`MeteringError::UnknownZone`] when the compiled tzdb does not know `zone`.
    pub fn calendar_month(zone: impl Into<String>) -> Result<Self, MeteringError> {
        let zone = zone.into();
        Self::check_zone(&zone)?;
        Ok(Self {
            zone,
            basis: PeriodBasis::CalendarMonth,
        })
    }

    /// Licence-anniversary windows in `zone`, starting on `anchor_day` each month.
    ///
    /// # Errors
    /// [`MeteringError::UnknownZone`] for an unknown zone;
    /// [`MeteringError::InvalidPeriodSpec`] when `anchor_day` is not in 1..=31.
    pub fn anniversary(zone: impl Into<String>, anchor_day: u8) -> Result<Self, MeteringError> {
        let zone = zone.into();
        Self::check_zone(&zone)?;
        if !(1..=31).contains(&anchor_day) {
            return Err(MeteringError::InvalidPeriodSpec {
                reason: format!("anchor day {anchor_day} is not in 1..=31"),
            });
        }
        Ok(Self {
            zone,
            basis: PeriodBasis::Anniversary { anchor_day },
        })
    }

    fn check_zone(zone: &str) -> Result<(), MeteringError> {
        jiff::tz::TimeZone::get(zone)
            .map(|_| ())
            .map_err(|_| MeteringError::UnknownZone {
                zone: zone.to_owned(),
            })
    }

    /// The IANA zone name the spec buckets in.
    #[must_use]
    pub fn zone(&self) -> &str {
        &self.zone
    }

    /// The basis.
    #[must_use]
    pub fn basis(&self) -> PeriodBasis {
        self.basis
    }
}

/// One concrete period window: `[start, end_exclusive)`, with a human label.
///
/// Two keys are equal when they name the same window (same zone, basis and label);
/// `start` and `end_exclusive` are derived, but carried so stores can bucket without
/// recomputing zone math.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeriodKey {
    /// The window label: `YYYY-MM` for calendar months, `YYYY-MM-DD` (the window's
    /// start date) for anniversary windows. Labels sort chronologically.
    pub label: String,
    /// The IANA zone the window was computed in.
    pub zone: String,
    /// The basis that produced the window.
    pub basis: PeriodBasis,
    /// The first second of the window, UTC seconds.
    pub start: i64,
    /// One past the last second of the window, UTC seconds; the next window's start.
    pub end_exclusive: i64,
}

impl PeriodKey {
    /// Whether `at` falls inside this window.
    #[must_use]
    pub fn contains(&self, at: i64) -> bool {
        self.start <= at && at < self.end_exclusive
    }
}

impl std::fmt::Display for PeriodKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{}]", self.label, self.zone)
    }
}

/// The period window `at` falls into, per `spec`.
///
/// Exactly one bucket per instant: the window is half-open and every boundary is
/// computed by the same zone math, so at a DST change or a month end the instant
/// lands in exactly one window — never in two, never in none.
///
/// # Errors
/// [`MeteringError::UnknownZone`] when the spec's zone is unknown;
/// [`MeteringError::PeriodUnresolvable`] when `at` is not a representable instant.
pub fn period_of(at: i64, spec: &PeriodSpec) -> Result<PeriodKey, MeteringError> {
    let tz = jiff::tz::TimeZone::get(spec.zone()).map_err(|_| MeteringError::UnknownZone {
        zone: spec.zone().to_owned(),
    })?;
    let ts = Timestamp::from_second(at).map_err(|_| MeteringError::PeriodUnresolvable {
        at,
        zone: spec.zone().to_owned(),
    })?;
    let local = ts.to_zoned(tz.clone()).date();

    let start_date = match spec.basis() {
        PeriodBasis::CalendarMonth => date_of(local.year(), local.month(), 1)?,
        PeriodBasis::Anniversary { anchor_day } => {
            let clamped = anchor_day.min(local.days_in_month() as u8);
            let this_anchor = date_of(local.year(), local.month(), clamped)?;
            if local >= this_anchor {
                this_anchor
            } else {
                let (prev_year, prev_month) = previous_month(local.year(), local.month());
                let prev_clamped = anchor_day.min(days_in(prev_year, prev_month)?);
                date_of(prev_year, prev_month, prev_clamped)?
            }
        }
    };
    let (next_year, next_month) = next_month(start_date.year(), start_date.month());
    let end_date = match spec.basis() {
        PeriodBasis::CalendarMonth => date_of(next_year, next_month, 1)?,
        PeriodBasis::Anniversary { anchor_day } => {
            let next_clamped = anchor_day.min(days_in(next_year, next_month)?);
            date_of(next_year, next_month, next_clamped)?
        }
    };

    let start = start_date
        .to_zoned(tz.clone())
        .map_err(|_| MeteringError::PeriodUnresolvable {
            at,
            zone: spec.zone().to_owned(),
        })?;
    let end = end_date
        .to_zoned(tz)
        .map_err(|_| MeteringError::PeriodUnresolvable {
            at,
            zone: spec.zone().to_owned(),
        })?;

    let label = match spec.basis() {
        PeriodBasis::CalendarMonth => {
            format!(
                "{:04}-{:02}",
                start_date.year(),
                i32::from(start_date.month())
            )
        }
        PeriodBasis::Anniversary { .. } => format!(
            "{:04}-{:02}-{:02}",
            start_date.year(),
            i32::from(start_date.month()),
            i32::from(start_date.day())
        ),
    };

    Ok(PeriodKey {
        label,
        zone: spec.zone().to_owned(),
        basis: spec.basis(),
        start: start.timestamp().as_second(),
        end_exclusive: end.timestamp().as_second(),
    })
}

fn date_of(year: i16, month: i8, day: u8) -> Result<Date, MeteringError> {
    Date::new(year, month, i8::try_from(day).unwrap_or(1)).map_err(|_| {
        MeteringError::InvalidPeriodSpec {
            reason: format!("invalid date {year}-{month}-{day}"),
        }
    })
}

fn days_in(year: i16, month: i8) -> Result<u8, MeteringError> {
    Ok(date_of(year, month, 1)?.days_in_month() as u8)
}

fn next_month(year: i16, month: i8) -> (i16, i8) {
    if month >= 12 {
        (year.saturating_add(1), 1)
    } else {
        (year, month + 1)
    }
}

fn previous_month(year: i16, month: i8) -> (i16, i8) {
    if month <= 1 {
        (year.saturating_sub(1), 12)
    } else {
        (year, month - 1)
    }
}

/// Which period a tenant's instant belongs to: the one place zone policy lives.
///
/// A store never guesses a tenant's zone: it is handed a resolver, so the same store
/// contract works for the in-memory implementation, a later PostgreSQL backend, and
/// the tests.
pub trait PeriodResolver: Send + Sync {
    /// The window `at` falls into for `tenant`.
    ///
    /// # Errors
    /// Whatever the implementation's zone resolution can fail with.
    fn resolve(&self, tenant: &str, at: i64) -> Result<PeriodKey, MeteringError>;
}

/// Per-tenant period specs with a default, the standard [`PeriodResolver`].
///
/// Calendar months in each tenant's own zone are the default (a lease's tenant list
/// carries a zone per tenant); a licence-anniversary window is set per tenant where a
/// licence's billing cycle is not the calendar month.
#[derive(Clone, Debug)]
pub struct TenantPeriods {
    default_spec: PeriodSpec,
    per_tenant: std::collections::BTreeMap<String, PeriodSpec>,
}

impl TenantPeriods {
    /// A resolver using `default_spec` for every tenant it has no override for.
    #[must_use]
    pub fn new(default_spec: PeriodSpec) -> Self {
        Self {
            default_spec,
            per_tenant: std::collections::BTreeMap::new(),
        }
    }

    /// Sets one tenant's own spec; builders keep chaining.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>, spec: PeriodSpec) -> Self {
        self.per_tenant.insert(tenant.into(), spec);
        self
    }

    /// The spec `tenant` resolves with.
    #[must_use]
    pub fn spec_for(&self, tenant: &str) -> &PeriodSpec {
        self.per_tenant.get(tenant).unwrap_or(&self.default_spec)
    }
}

impl PeriodResolver for TenantPeriods {
    fn resolve(&self, tenant: &str, at: i64) -> Result<PeriodKey, MeteringError> {
        period_of(at, self.spec_for(tenant))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tenant_resolver_prefers_the_override() {
        let resolver = TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap())
            .with_tenant("acme", PeriodSpec::anniversary("Asia/Karachi", 15).unwrap());
        // 2026-01-20T00:00:00Z is the Karachi window starting Jan 15.
        let acme = resolver.resolve("acme", 1_768_860_800).unwrap();
        assert_eq!(acme.label, "2026-01-15");
        assert_eq!(acme.zone, "Asia/Karachi");
        // Everyone else buckets by the UTC calendar month.
        let other = resolver.resolve("other", 1_768_860_800).unwrap();
        assert_eq!(other.label, "2026-01");
        assert_eq!(other.zone, "UTC");
    }

    fn utc(spec: &PeriodSpec, at: i64) -> PeriodKey {
        period_of(at, spec).unwrap()
    }

    #[test]
    fn calendar_month_buckets_in_utc_seconds() {
        let spec = PeriodSpec::calendar_month("UTC").unwrap();
        let key = utc(&spec, 0);
        assert_eq!(key.label, "1970-01");
        assert_eq!(key.start, 0);
        assert_eq!(key.end_exclusive, 31 * 86_400);
        assert!(key.contains(0) && key.contains(31 * 86_400 - 1));
        assert!(!key.contains(31 * 86_400));
    }

    #[test]
    fn a_zone_with_a_half_hour_offset_starts_its_month_earlier_in_utc() {
        // Asia/Kolkata is +05:30 with no DST: March starts at Feb 29 18:30 UTC.
        let spec = PeriodSpec::calendar_month("Asia/Kolkata").unwrap();
        let feb_last = utc(&spec, 1_709_231_399); // 2024-02-29 18:29:59Z
        let mar_first = utc(&spec, 1_709_231_400); // 2024-02-29 18:30:00Z
        assert_eq!(feb_last.label, "2024-02");
        assert_eq!(mar_first.label, "2024-03");
        assert_eq!(mar_first.start, feb_last.end_exclusive);
    }

    #[test]
    fn a_leap_day_stays_inside_february() {
        let spec = PeriodSpec::calendar_month("UTC").unwrap();
        let key = utc(&spec, 1_709_164_800); // 2024-02-29 00:00:00Z
        assert_eq!(key.label, "2024-02");
        // 2024 is a leap year: February has 29 days.
        assert_eq!(key.end_exclusive - key.start, 29 * 86_400);
    }

    #[test]
    fn anniversary_windows_clamp_to_short_months() {
        // Anchor on the 31st: January's window runs Jan 31 .. Feb 28 (2023, not leap).
        let spec = PeriodSpec::anniversary("UTC", 31).unwrap();
        let feb = utc(&spec, 1_709_164_800); // 2024-02-29: leap year, so anchor is 29
        assert_eq!(feb.label, "2024-02-29");
        // 2023 (not a leap year): the anchor clamps to Feb 28.
        let spec23 = PeriodSpec::anniversary("UTC", 31).unwrap();
        let feb23 = utc(&spec23, 1_677_580_800); // 2023-03-01 00:00:00Z -> window starting Feb 28
        assert_eq!(feb23.label, "2023-02-28");
        assert_eq!(feb23.end_exclusive - feb23.start, 31 * 86_400); // Feb 28 .. Mar 31
    }

    #[test]
    fn every_instant_lands_in_exactly_one_window_across_a_dst_change() {
        // Europe/Dublin springs forward on the last Sunday of March at 01:00 local.
        let spec = PeriodSpec::calendar_month("Europe/Dublin").unwrap();
        // Sweep hourly across the 2026 transition (Mar 29) and the month end.
        let from = 1_774_700_000; // 2026-03-28 ~00:00Z
        let to = 1_775_200_000; // 2026-04-02 ~
        let mut previous: Option<PeriodKey> = None;
        let mut at = from;
        while at < to {
            let key = utc(&spec, at);
            assert!(key.contains(at), "window {key} must contain {at}");
            if let Some(prev) = &previous
                && prev.label != key.label
            {
                // Window boundaries partition the line: the new window starts
                // exactly where the old one ended.
                assert_eq!(key.start, prev.end_exclusive, "gap at {at}");
            }
            previous = Some(key);
            at += 3_600;
        }
    }

    #[test]
    fn unknown_zones_are_refused_at_construction() {
        assert!(PeriodSpec::calendar_month("Mars/Olympus").is_err());
        assert!(PeriodSpec::anniversary("UTC", 0).is_err());
        assert!(PeriodSpec::anniversary("UTC", 32).is_err());
    }
}
