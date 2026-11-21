//! Small TOML rendering and inspection helpers shared by the text form, the
//! content hash and validation.
//!
//! The text form is canonical: object keys sorted, one scalar per line, nested
//! tables and arrays written inline, no extra whitespace. The same writer is
//! used for hashing, so identical content renders to identical bytes.

use toml::Value;

/// Quotes a string as a TOML basic string, escaping only what TOML allows
/// (`\"`, `\\`, `\n`, `\t`, `\r` and `\uXXXX` for other control characters).
#[must_use]
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders a value inline (a single line): a table as `{ k = v, ... }`, an
/// array as `[v, ...]`, a scalar in its TOML literal form.
#[must_use]
pub fn render_value(value: &Value) -> String {
    match value {
        Value::String(s) => quote(s),
        Value::Integer(i) => i.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Datetime(dt) => dt.to_string(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(render_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Table(table) => render_inline_table(table),
    }
}

/// Renders a table inline with its keys sorted: `{ a = 1, b = "x" }`.
#[must_use]
pub fn render_inline_table(table: &toml::Table) -> String {
    let mut keys: Vec<&String> = table.keys().collect();
    keys.sort();
    let parts: Vec<String> = keys
        .iter()
        .map(|k| format!("{} = {}", k, render_value(&table[k.as_str()])))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// Renders a table as canonical `key = value` lines, keys sorted ascending.
#[must_use]
pub fn render_key_values(table: &toml::Table) -> Vec<String> {
    let mut keys: Vec<&String> = table.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| format!("{} = {}", k, render_value(&table[k.as_str()])))
        .collect()
}

/// The path of the first float inside `value`, or `None` when there is none
/// (ADR-12: config numbers must be integers).
#[must_use]
pub fn find_float(value: &Value, path: &str) -> Option<String> {
    match value {
        Value::Float(_) => Some(path.to_owned()),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| {
            let child = format!("{path}[{i}]");
            find_float(item, &child)
        }),
        Value::Table(table) => table.iter().find_map(|(k, item)| {
            let child = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            find_float(item, &child)
        }),
        _ => None,
    }
}
