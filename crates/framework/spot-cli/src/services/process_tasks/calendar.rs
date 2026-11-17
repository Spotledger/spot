//! The `[services.process_tasks]` settings and the working calendar SLA time is counted on.

use std::time::Duration;

use serde::Deserialize;
use spot_bpm_tasks::BusinessCalendar;
use time::{OffsetDateTime, Time, UtcOffset, Weekday};

/// Most working days `walk` crosses before it gives up (about sixteen years): bounds the loop.
const MAX_CALENDAR_STEPS: usize = 4_000;

/// `[services.process_tasks]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all. `false`: no table, no port, no route.
    pub enabled: bool,
    /// How often the SLA timers are checked, in seconds. Bounded 5 to 300.
    pub tick_secs: u64,
    /// The working calendar SLA offsets are counted on; `None` counts wall time.
    pub calendar: Option<CalendarSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_secs: 15,
            calendar: None,
        }
    }
}

impl Settings {
    /// Checks the bounds and the calendar, naming the first setting that is out of range.
    ///
    /// # Errors
    /// The name of the setting and why it is refused.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(5..=300).contains(&self.tick_secs) {
            return Err("tick_secs: must be 5 to 300".to_owned());
        }
        if let Some(calendar) = &self.calendar {
            WorkCalendar::parse(calendar).map_err(|why| format!("calendar: {why}"))?;
        }
        Ok(())
    }
}

/// `[services.process_tasks.calendar]`: the days and hours SLA time passes in.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CalendarSettings {
    /// Working days, `mon` to `sun`.
    pub days: Vec<String>,
    /// The first working minute of a day, `HH:MM`.
    pub start: String,
    /// The end of the working day, `HH:MM`, after `start`.
    pub end: String,
    /// The offset of the site's local time from UTC, in minutes.
    pub utc_offset_minutes: i32,
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self {
            days: ["mon", "tue", "wed", "thu", "fri"]
                .iter()
                .map(|d| (*d).to_owned())
                .collect(),
            start: "09:00".to_owned(),
            end: "17:00".to_owned(),
            utc_offset_minutes: 0,
        }
    }
}

/// A working calendar: SLA seconds are counted only inside the working hours of working days.
#[derive(Clone, Debug)]
pub struct WorkCalendar {
    days: Vec<Weekday>,
    open: Time,
    close: Time,
    offset: UtcOffset,
}

fn parse_weekday(text: &str) -> Option<Weekday> {
    match text.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Some(Weekday::Monday),
        "tue" | "tuesday" => Some(Weekday::Tuesday),
        "wed" | "wednesday" => Some(Weekday::Wednesday),
        "thu" | "thursday" => Some(Weekday::Thursday),
        "fri" | "friday" => Some(Weekday::Friday),
        "sat" | "saturday" => Some(Weekday::Saturday),
        "sun" | "sunday" => Some(Weekday::Sunday),
        _ => None,
    }
}

fn parse_clock(text: &str) -> Option<Time> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let hours: u8 = hours.parse().ok()?;
    let minutes: u8 = minutes.parse().ok()?;
    Time::from_hms(hours, minutes, 0).ok()
}

impl WorkCalendar {
    /// Parses the settings.
    ///
    /// # Errors
    /// No working day, an unknown day, a time that is not `HH:MM`, an end that is not after the
    /// start, or an offset beyond one day.
    pub fn parse(settings: &CalendarSettings) -> std::result::Result<Self, String> {
        if settings.days.is_empty() {
            return Err("days: at least one working day".to_owned());
        }
        let mut days = Vec::new();
        for text in &settings.days {
            let day = parse_weekday(text).ok_or_else(|| "days: not a weekday".to_owned())?;
            if !days.contains(&day) {
                days.push(day);
            }
        }
        let open = parse_clock(&settings.start).ok_or_else(|| "start: not HH:MM".to_owned())?;
        let close = parse_clock(&settings.end).ok_or_else(|| "end: not HH:MM".to_owned())?;
        if close <= open {
            return Err("end: must be after start".to_owned());
        }
        let seconds = i32::try_from(i64::from(settings.utc_offset_minutes) * 60)
            .map_err(|_| "utc_offset_minutes: out of range".to_owned())?;
        let offset = UtcOffset::from_whole_seconds(seconds)
            .map_err(|_| "utc_offset_minutes: out of range".to_owned())?;
        Ok(Self {
            days,
            open,
            close,
            offset,
        })
    }

    /// The first working moment at or after `at`, or `None` when none is found within a fortnight.
    fn next_working(&self, at: OffsetDateTime) -> Option<OffsetDateTime> {
        let mut at = at;
        for _ in 0..15 {
            if self.days.contains(&at.weekday()) {
                let open = at.replace_time(self.open);
                let close = at.replace_time(self.close);
                if at < open {
                    return Some(open);
                }
                if at < close {
                    return Some(at);
                }
            }
            let next = at.date().next_day()?;
            at = at.replace_date(next).replace_time(Time::MIDNIGHT);
        }
        None
    }

    fn walk(&self, start: OffsetDateTime, seconds: u64) -> Option<OffsetDateTime> {
        let mut remaining = i64::try_from(seconds).ok()?;
        let mut at = start.to_offset(self.offset);
        for _ in 0..MAX_CALENDAR_STEPS {
            at = self.next_working(at)?;
            let close = at.replace_time(self.close);
            let available = (close - at).whole_seconds();
            if remaining <= available {
                return Some(at + time::Duration::seconds(remaining));
            }
            remaining -= available;
            at = close;
        }
        None
    }
}

impl BusinessCalendar for WorkCalendar {
    fn instant_after(&self, start: OffsetDateTime, seconds: u64) -> OffsetDateTime {
        self.walk(start, seconds)
            .unwrap_or_else(|| start + Duration::from_secs(seconds))
    }
}
