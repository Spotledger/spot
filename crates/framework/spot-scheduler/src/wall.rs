//! Wall-clock firing: cron candidates are local wall times, and each is resolved to one instant
//! by jiff's compiled IANA rules, so the DST policy lives in one place, [`resolve`].
//!
//! `cron` 0.12 enumerates candidates over a chrono time zone. Handing it a DST-aware zone would
//! make it drop every candidate that falls in a gap or a fold (its `ymd(..).and_hms_opt(..)`
//! path wants exactly one instant per wall time), which skips the job for the day. Instead the
//! enumeration runs over naive wall times (chrono `Utc` is used only as a label that has no
//! transitions), and this module decides what each wall time means:
//!
//! - **unambiguous**: the instant itself;
//! - **gap** (spring forward, the wall time does not exist): the instant the same wall reading
//!   has under the offset in force before the gap, which is the first valid instant at or after
//!   the gap's end: the job runs **once**, late;
//! - **fold** (fall back, the wall time happens twice): the **first** occurrence; the second is
//!   never fired because the search is strictly after the instant it returned.

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use jiff::Timestamp;
use jiff::civil::DateTime as Civil;
use jiff::tz::{AmbiguousOffset, TimeZone};

/// The first firing of `program` strictly after `after` and not past `search_end`, in `zone`.
///
/// `None` when the program never fires again in that window.
pub(crate) fn next_after(
    program: &cron::Schedule,
    zone: &TimeZone,
    after: Timestamp,
    search_end: Timestamp,
) -> Option<Timestamp> {
    // Local wall reading of `after`, to the second: a candidate must be strictly later.
    let start = naive_of(zone.to_datetime(after))?.and_utc();
    for fire in program.after_owned(start) {
        let stamp = resolve(zone, civil_of(&fire.naive_utc())?)?;
        if stamp > search_end {
            return None;
        }
        if stamp > after {
            return Some(stamp);
        }
    }
    None
}

/// One instant for a local wall time under the DST policy in the module docs.
pub(crate) fn resolve(zone: &TimeZone, wall: Civil) -> Option<Timestamp> {
    match zone.to_ambiguous_timestamp(wall).offset() {
        AmbiguousOffset::Unambiguous { offset }
        | AmbiguousOffset::Gap { before: offset, .. }
        | AmbiguousOffset::Fold { before: offset, .. } => offset.to_timestamp(wall).ok(),
    }
}

/// A jiff civil datetime as a naive chrono one, whole seconds.
fn naive_of(civil: Civil) -> Option<NaiveDateTime> {
    NaiveDate::from_ymd_opt(
        i32::from(civil.year()),
        u32::try_from(civil.month()).ok()?,
        u32::try_from(civil.day()).ok()?,
    )?
    .and_hms_opt(
        u32::try_from(civil.hour()).ok()?,
        u32::try_from(civil.minute()).ok()?,
        u32::try_from(civil.second()).ok()?,
    )
}

/// A naive chrono datetime as a jiff civil one.
fn civil_of(naive: &NaiveDateTime) -> Option<Civil> {
    Civil::new(
        i16::try_from(naive.year()).ok()?,
        i8::try_from(naive.month()).ok()?,
        i8::try_from(naive.day()).ok()?,
        i8::try_from(naive.hour()).ok()?,
        i8::try_from(naive.minute()).ok()?,
        i8::try_from(naive.second()).ok()?,
        0,
    )
    .ok()
}
