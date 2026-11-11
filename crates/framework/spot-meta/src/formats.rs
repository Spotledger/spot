//! Field formats (ADR-068 §2): which formats a base type allows, and the checks for the text
//! formats.
//!
//! A format is a validated, displayed interpretation of a base type; it never changes the
//! storage type. [`check_text_format`] is the one implementation of the text checks: this crate
//! runs it on `default` values, the document engine runs it on save (WP-DOC), and hosts and
//! tests call it directly. It has no dependencies.
//!
//! One format check is **not** here because it needs dependencies this crate does not have:
//! the sanitising of `html`, which the document engine owns. The GeoJSON check of the
//! deprecated `geolocation` alias is [`check_geolocation_point`]: it says whether a value can
//! move to the `geo` type, which holds one point and nothing else (ADR-087 section 4.1).

use crate::definition::FieldType;

/// The formats a base type allows, the default first. Empty for a type without formats.
pub fn formats_of(ty: FieldType) -> &'static [&'static str] {
    match ty {
        FieldType::Text => &["plain", "email", "phone", "url", "color", "barcode"],
        FieldType::LongText => &["plain", "markdown", "html", "code"],
        FieldType::Int => &["plain", "duration", "rating"],
        FieldType::Decimal => &["plain", "currency", "percent"],
        FieldType::Json => &["plain", "geolocation"],
        FieldType::Geo => &["point"],
        FieldType::Range => &["date_range", "instant_range"],
        FieldType::Attach => &["file", "image", "signature"],
        _ => &[],
    }
}

/// Checks `value` against a text format: `plain`, `email`, `phone`, `url`, `color` or
/// `barcode`. Any other format name is an error, so a caller cannot pass a typo through.
///
/// The rules are deliberately simple and syntactic; they do not look anything up.
pub fn check_text_format(format: &str, value: &str) -> Result<(), String> {
    match format {
        "plain" => Ok(()),
        "email" => email(value),
        "phone" => phone(value),
        "url" => url(value),
        "color" => color(value),
        "barcode" => barcode(value),
        other => Err(format!("`{other}` is not a text format")),
    }
}

fn email(v: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("`{v}` is not an e-mail address: {why}"));
    if v.chars().count() > 254 {
        return bad("longer than 254 characters");
    }
    if v.chars().any(char::is_whitespace) {
        return bad("it contains a space");
    }
    let mut parts = v.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return bad("it needs exactly one `@`");
    };
    if local.is_empty() {
        return bad("the part before `@` is empty");
    }
    if !domain.contains('.') || domain.split('.').any(str::is_empty) {
        return bad("the domain needs a dot and no empty part");
    }
    Ok(())
}

fn phone(v: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("`{v}` is not a phone number: {why}"));
    let len = v.chars().count();
    if !(5..=20).contains(&len) {
        return bad("it must be 5 to 20 characters");
    }
    let body = v.strip_prefix('+').unwrap_or(v);
    if !body
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')'))
    {
        return bad("only digits, spaces, `-`, `(` and `)` may follow an optional `+`");
    }
    if body.chars().filter(char::is_ascii_digit).count() < 5 {
        return bad("it needs at least 5 digits");
    }
    Ok(())
}

fn url(v: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("`{v}` is not a URL: {why}"));
    if v.chars().any(char::is_whitespace) {
        return bad("it contains a space");
    }
    let rest = ["https://", "http://", "mailto:"]
        .iter()
        .find_map(|scheme| v.strip_prefix(scheme));
    match rest {
        None => bad("the scheme must be http, https or mailto"),
        Some("") => bad("nothing follows the scheme"),
        Some(_) => Ok(()),
    }
}

fn color(v: &str) -> Result<(), String> {
    let hex = v.strip_prefix('#').unwrap_or("");
    if matches!(hex.len(), 3 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!(
            "`{v}` is not a colour: use #RGB, #RRGGBB or #RRGGBBAA"
        ))
    }
}

fn barcode(v: &str) -> Result<(), String> {
    if (1..=64).contains(&v.len()) && v.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        Ok(())
    } else {
        Err(format!(
            "`{v}` is not a barcode value: 1 to 64 printable ASCII characters"
        ))
    }
}

/// Checks that `text` is GeoJSON holding a single `Point` that the `geo` type can store, and
/// returns the point. This is the check that decides whether a value of the deprecated
/// `json`/`geolocation` alias can move to `geo`/`point` (ADR-087 section 4.1): a polygon, a
/// collection or anything else is refused, never narrowed, because narrowing would lose data.
///
/// Only a `Point` with two coordinates is accepted (longitude, then latitude): a third
/// (altitude) is refused as not a plain point, as is a coordinate outside the WGS-84 range or
/// with more than seven decimal places. The `Err` is a sentence for the author; the code it
/// travels under is `meta.geolocation_not_a_point`.
pub fn check_geolocation_point(text: &str) -> Result<spot_storage_contract::GeoPoint, String> {
    use std::str::FromStr;

    use spot_app_api::prelude::Decimal;

    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("the value is not JSON: {e}"))?;
    let kind = value.get("type").and_then(serde_json::Value::as_str);
    if kind != Some("Point") {
        return Err(format!(
            "the value is GeoJSON of type {}, not a single `Point`; the `geo` type holds one point",
            kind.map_or_else(|| "(none)".to_owned(), |k| format!("`{k}`"))
        ));
    }
    let coords = value
        .get("coordinates")
        .and_then(serde_json::Value::as_array)
        .ok_or("the `Point` has no `coordinates` array")?;
    let [lon, lat] = coords.as_slice() else {
        return Err("a `Point` needs exactly two coordinates, longitude then latitude".into());
    };
    let number = |v: &serde_json::Value, what: &str| -> Result<Decimal, String> {
        match v {
            serde_json::Value::Number(n) => Decimal::from_str(&n.to_string())
                .map_err(|_| format!("the {what} `{n}` is not a plain decimal number")),
            _ => Err(format!("the {what} is not a number")),
        }
    };
    let point =
        spot_storage_contract::GeoPoint::new(number(lon, "longitude")?, number(lat, "latitude")?);
    point.validate()?;
    Ok(point)
}
