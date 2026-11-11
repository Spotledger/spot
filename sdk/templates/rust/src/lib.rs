//! The reference guest for the SDK conformance harness (WP-STR-07.T1).
//!
//! It implements the sample app over the `spot-wasm` test world (`wit/world.wit`): the export
//! `on-event(event: string) -> result<string, error>` and no imports (the world declares `log`,
//! `now` and `get-doc`, but this guest does not call them, so the component does not import them).
//!
//! Built for `wasm32-unknown-unknown` and componentized with `wasm-tools component new`, so it
//! carries **no WASI** and links on the product host (`crates/framework/spot-wasm`), which
//! provides no WASI. This is the route SPK-05 found for Rust; a `wasm32-wasip2` `std` guest
//! imports the WASI 0.2 CLI world for its panic and OOM plumbing and would not link here.
//!
//! The behaviour is the sample app reduced to one DocType, one lifecycle step and one
//! script-callable function. The event string is a JSON envelope:
//!
//! ```text
//! {"op":"step","name":"sample:check_hours","hours":"2.5"}
//! {"op":"call","name":"sample:double_hours","hours":"2.5"}
//! ```
//!
//! Money and quantities are never floating point (ADR-12): hours are parsed to **hundredths**
//! and formatted back, all in integer arithmetic.

wit_bindgen::generate!({ world: "test-world", path: "wit" });

/// The lowest bookable hours, in hundredths (0.25).
const HOURS_MIN: i64 = 25;

/// The highest bookable hours, in hundredths (24.00).
const HOURS_MAX: i64 = 2400;

struct Component;

impl Guest for Component {
    fn on_event(event: String) -> Result<String, Error> {
        let op = field(&event, "op").unwrap_or("");
        let name = field(&event, "name").unwrap_or("");
        match (op, name) {
            ("step", "sample:check_hours") => check_hours(field(&event, "hours")),
            ("call", "sample:double_hours") => double_hours(field(&event, "hours")),
            _ => Err(refusal(
                "sample.unknown_op",
                &format!("no operation `{op}:{name}`"),
            )),
        }
    }
}

/// The lifecycle step: the hours range, the same check as the sample app's `check_hours`.
fn check_hours(hours: Option<&str>) -> Result<String, Error> {
    let value = hours
        .and_then(parse_hours)
        .ok_or_else(|| refusal("sample.bad_hours", "hours must be a decimal string"))?;
    if value < HOURS_MIN || value > HOURS_MAX {
        return Err(refusal(
            "sample.hours_out_of_range",
            "Hours must be between 0.25 and 24.",
        ));
    }
    Ok("{\"ok\":true,\"step\":\"sample:check_hours\"}".to_owned())
}

/// The script-callable function: the hours doubled, as a canonical two-place decimal.
fn double_hours(hours: Option<&str>) -> Result<String, Error> {
    let value = hours
        .and_then(parse_hours)
        .ok_or_else(|| refusal("sample.bad_hours", "hours must be a decimal string"))?;
    Ok(format!(
        "{{\"value\":\"{}\"}}",
        format_hours(value.saturating_mul(2))
    ))
}

/// The app's error record, the world's `error`.
fn refusal(code: &str, message: &str) -> Error {
    Error {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// Reads a JSON string field's value: `"key":"value"`. The envelope carries only string values,
/// so a small scanner keeps the guest free of a JSON dependency (and of `std`'s WASI imports).
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Parses a non-negative decimal with at most two places into hundredths, or `None`.
fn parse_hours(text: &str) -> Option<i64> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if fraction.len() > 2 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let fraction: i64 = match fraction.len() {
        0 => 0,
        1 => format!("{fraction}0").parse().ok()?,
        _ => fraction.parse().ok()?,
    };
    whole.checked_mul(100)?.checked_add(fraction)
}

/// Formats hundredths as a canonical two-place decimal.
fn format_hours(hundredths: i64) -> String {
    let sign = if hundredths < 0 { "-" } else { "" };
    let magnitude = hundredths.abs();
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

export!(Component);
