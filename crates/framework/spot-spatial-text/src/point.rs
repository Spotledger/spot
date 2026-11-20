//! `POINT(<lon> <lat>)`.

use spot_storage_contract::GeoPoint;

use crate::error::{Reason, TextError};
use crate::num;

/// The decimal places a coordinate may carry (ADR-087).
const MAX_PLACES: u32 = 7;

/// `POINT(<lon> <lat>)`: longitude first, each number as the `Decimal` holds it after
/// `normalize` (no padding, no exponent).
#[must_use]
pub fn format_point(point: &GeoPoint) -> String {
    format!("POINT({} {})", point.lon.normalize(), point.lat.normalize())
}

/// Parses `POINT(<lon> <lat>)`: the keyword in any case, spaces around the parentheses and
/// between the numbers, a dot as the decimal separator.
///
/// # Errors
///
/// A [`TextError`] with the reason: `Syntax`, `TooManyDecimals` or `OutOfRange`.
pub fn parse_point(cell: &str) -> Result<GeoPoint, TextError> {
    let text = cell.trim();
    let syntax = || TextError::new(Reason::Syntax, "a position is `POINT(lon lat)`", cell);
    let head = text.get(..5).ok_or_else(syntax)?;
    if !head.eq_ignore_ascii_case("POINT") {
        return Err(syntax());
    }
    let rest = text[5..].trim_start();
    let inner = rest
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .ok_or_else(syntax)?;
    let mut numbers = inner.split_whitespace();
    let (Some(lon), Some(lat), None) = (numbers.next(), numbers.next(), numbers.next()) else {
        return Err(syntax());
    };
    let lon = num::decimal(lon).ok_or_else(syntax)?;
    let lat = num::decimal(lat).ok_or_else(syntax)?;
    for value in [lon, lat] {
        if value.normalize().scale() > MAX_PLACES {
            return Err(TextError::new(
                Reason::TooManyDecimals,
                "a coordinate has at most 7 decimal places",
                cell,
            ));
        }
    }
    let point = GeoPoint::new(lon, lat);
    point.validate().map_err(|_| {
        TextError::new(
            Reason::OutOfRange,
            "longitude is -180 to 180 and latitude -90 to 90 degrees",
            cell,
        )
    })?;
    Ok(point)
}
