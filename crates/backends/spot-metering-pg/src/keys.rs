//! How meters, numbers, period bases and the retention floor are written to and read from rows.

use spot_metering::{Meter, MeteringError, PeriodBasis, PeriodKey};
use tokio_postgres::Row;

use crate::error::corrupt;

/// The prefix that keeps an app-declared meter apart from a built-in with the same name.
const CUSTOM_PREFIX: &str = "custom:";

/// The row key of a meter: the built-in name, or `custom:` plus the app's name. The two never
/// collide, so `Custom("transactions")` and `Transactions` stay different meters, as they are
/// in the in-memory store.
pub(crate) fn meter_key(meter: &Meter) -> String {
    match meter {
        Meter::Custom(name) => format!("{CUSTOM_PREFIX}{name}"),
        builtin => builtin.name().to_owned(),
    }
}

/// The meter behind a row key.
pub(crate) fn meter_from_key(key: &str) -> Result<Meter, MeteringError> {
    if let Some(name) = key.strip_prefix(CUSTOM_PREFIX) {
        return Ok(Meter::Custom(name.to_owned()));
    }
    Meter::builtins()
        .into_iter()
        .find(|meter| meter.name() == key)
        .ok_or_else(|| corrupt("unknown meter"))
}

/// The period basis behind the key [`PeriodBasis::label`] writes.
pub(crate) fn basis_from_key(key: &str) -> Result<PeriodBasis, MeteringError> {
    if key == "calendar" {
        return Ok(PeriodBasis::CalendarMonth);
    }
    key.strip_prefix("anniversary:")
        .and_then(|day| day.parse::<u8>().ok())
        .filter(|anchor_day| (1..=31).contains(anchor_day))
        .map(|anchor_day| PeriodBasis::Anniversary { anchor_day })
        .ok_or_else(|| corrupt("basis"))
}

/// A `numeric(20,0)` column read as text, back to `u64`.
pub(crate) fn parse_u64(text: &str) -> Result<u64, MeteringError> {
    text.parse::<u64>().map_err(|_| corrupt("number"))
}

/// A non-negative `bigint` count, as `u64`.
pub(crate) fn count_u64(value: i64) -> Result<u64, MeteringError> {
    u64::try_from(value).map_err(|_| corrupt("count"))
}

/// The retention row, back to the period it names, or `None` for the no-floor sentinel
/// [`crate::sql::ENSURE_FLOOR_ROW`] writes for a tenant that has never been compacted.
///
/// Columns: `period_start`, `period_end`, `label`, `zone`, `basis`.
pub(crate) fn floor_from_row(row: &Row) -> Result<Option<PeriodKey>, MeteringError> {
    let start = row.try_get(0).map_err(|_| corrupt("floor start"))?;
    if start == crate::sql::NO_FLOOR_START {
        return Ok(None);
    }
    let end_exclusive = row.try_get(1).map_err(|_| corrupt("floor end"))?;
    let label = row.try_get(2).map_err(|_| corrupt("floor label"))?;
    let zone = row.try_get(3).map_err(|_| corrupt("floor zone"))?;
    let basis: String = row.try_get(4).map_err(|_| corrupt("floor basis"))?;
    Ok(Some(PeriodKey {
        label,
        zone,
        basis: basis_from_key(&basis)?,
        start,
        end_exclusive,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_basis_label_round_trips() {
        for basis in [
            PeriodBasis::CalendarMonth,
            PeriodBasis::Anniversary { anchor_day: 15 },
            PeriodBasis::Anniversary { anchor_day: 31 },
        ] {
            assert_eq!(basis_from_key(&basis.label()).ok(), Some(basis));
        }
    }

    #[test]
    fn an_unknown_basis_is_a_corrupt_row() {
        for key in ["", "monthly", "anniversary:", "anniversary:99"] {
            assert_eq!(
                basis_from_key(key).map_err(|e| e.code()),
                Err("metering.store_unavailable")
            );
        }
    }
}
