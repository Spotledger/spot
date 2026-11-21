//! The canonical JSON encoding of commands (brief §2).
//!
//! A command is signed and verified on two machines that share no implementation detail
//! beyond this crate, so both sides must produce **the same bytes for the same command**:
//! `serde_json`'s default writer does not (map order, spacing, float spelling), so the
//! encoding is written here, once, deterministically.
//!
//! # The exact rules
//!
//! The encoding is the subset of RFC 8785 (JCS) that a JSON command needs:
//!
//! 1. Objects: keys sorted by their UTF-8 byte sequences (RFC 8785 §3.2.3 sorts by UTF-16
//!    code units, which differs from UTF-8 ordering only above the BMP; this encoding
//!    rejects unpaired surrogates outright, so the two orders agree on every accepted
//!    string), no whitespace between tokens, entries as `"key":value`.
//! 2. Strings: minimal escaping — `"` becomes `\"`, `\` becomes `\\`, and the C0 control
//!    range U+0000–U+001F becomes `\u00xx` (hex, lower-case), with the short forms `\b`
//!    (U+0008), `\t` (U+0009), `\n` (U+000A), `\f` (U+000C) and `\r` (U+000D). Every other
//!    character is written as itself; non-ASCII is UTF-8, never escaped.
//! 3. Numbers: integers as plain digits (no exponent, no fraction, `i64`/`u64` range).
//!    **A float is refused with a typed error** because IEEE-754 has no canonical
//!    cross-machine spelling and the bytes are a signature commitment; money and
//!    quantities are decimal strings anyway (ADR-12), so no float ever belongs in a
//!    command.
//! 4. Unpaired UTF-16 surrogates (lone, or two that do not form a valid pair) never reach
//!    this writer — a `&str` cannot hold one — and `serde_json` refuses their escape
//!    forms on parse; a value built some other way that smuggles a replacement character
//!    in is written verbatim and then caught by the verifier's re-canonicalisation, which
//!    refuses any input whose bytes differ from the writer's output.
//! 5. `null`, `true`, `false` are the bare tokens; `,` and `:` carry no surrounding
//!    whitespace anywhere.
//!
//! The verifier re-runs this writer over the parsed command and demands byte-equality
//! with what was signed, so a non-canonical variant of the same command is refused before
//! any key is looked at.

use std::fmt;

use serde_json::Value;

/// Why the canonical writer refused a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NotCanonicalReason {
    /// A JSON number outside the integer range, a fractional or an exponent spelling:
    /// no safe canonical form exists, so the command is refused.
    Float,
}

impl fmt::Display for NotCanonicalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Float => f.write_str("a JSON number is a float, which has no canonical form"),
        }
    }
}

/// The canonical writer's failure: which rule the value broke. Mapped to
/// [`CommandError::NotCanonical`](crate::error::CommandError::NotCanonical) by every
/// caller; the reason exists so the unit tests can assert which rule fired.
#[derive(Debug, thiserror::Error)]
#[error("canonical json: {reason}")]
pub(crate) struct CanonicalJsonError {
    /// The rule the value broke, mapped to the public error by the caller.
    reason: NotCanonicalReason,
}

impl CanonicalJsonError {
    /// The rule the value broke; the caller maps it to the public error.
    pub(crate) fn reason(&self) -> &NotCanonicalReason {
        &self.reason
    }
}

/// Writes `value` in the canonical form the module docs state.
///
/// This is the crate's only JSON writer: signing, verification's re-canonicalisation and
/// the golden tests all go through it, so one implementation decides the bytes. It is
/// public so a caller embedding canonical fragments (audit rendering, a relay showing
/// "the equivalent command") produces the same bytes the signer signed.
///
/// # Errors
/// [`CommandError::NotCanonical`](crate::error::CommandError::NotCanonical) when the
/// value holds a JSON number without a canonical integer form.
pub fn canonical_json_value(value: &Value) -> Result<Vec<u8>, crate::error::CommandError> {
    let mut out = Vec::new();
    write_value(value, &mut out).map_err(|error| match error.reason() {
        NotCanonicalReason::Float => crate::error::CommandError::NotCanonical,
    })?;
    Ok(out)
}

/// Writes one value into `out`, the rules of the module docs in match order.
fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalJsonError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => {
            if number.as_i64().is_none() && number.as_u64().is_none() {
                // Out of the integer range: a float (or a big integer `serde_json`
                // degraded to one). No canonical form, refuse.
                return Err(CanonicalJsonError {
                    reason: NotCanonicalReason::Float,
                });
            }
            let text = number.to_string();
            debug_assert!(
                !text.contains(['.', 'e', 'E']),
                "an integer number must render as plain digits"
            );
            out.extend_from_slice(text.as_bytes());
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // `serde_json`'s default map is `BTreeMap<String, Value>` (the `preserve_order`
            // feature is off in this workspace), so iteration is already sorted by the
            // `String`'s byte order; the assertion keeps the canonicity guarantee honest
            // if the workspace feature set ever changes.
            debug_assert!(
                map.keys()
                    .zip(map.keys().skip(1))
                    .all(|(a, b)| a.as_bytes() < b.as_bytes()),
                "serde_json maps must stay sorted for the canonical encoding"
            );
            out.push(b'{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_value(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// Writes a string with the minimal escaping of the rules: quote, backslash, the C0
/// control range with the short forms, nothing else escaped.
fn write_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for unit in text.chars() {
        match unit {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{0008}' => out.extend_from_slice(b"\\b"),
            '\u{0009}' => out.extend_from_slice(b"\\t"),
            '\u{000A}' => out.extend_from_slice(b"\\n"),
            '\u{000C}' => out.extend_from_slice(b"\\f"),
            '\u{000D}' => out.extend_from_slice(b"\\r"),
            c if (c as u32) < 0x20 => {
                // The remaining C0 controls: `\u00xx`, hex digits lower-case.
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let code = c as u32;
                out.extend_from_slice(b"\\u00");
                out.push(HEX[((code >> 4) & 0xF) as usize]);
                out.push(HEX[(code & 0xF) as usize]);
            }
            c => {
                // Every other scalar, ASCII and non-ASCII, verbatim as UTF-8; `c` came
                // from a `&str`, so it cannot be a surrogate by construction.
                let mut buffer = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::canonical_json_value;
    use serde_json::json;

    /// Object keys sort recursively and no whitespace survives. Key order and shape are
    /// the contract both machines share; the golden bytes are fixed here.
    #[test]
    fn sorts_keys_recursively_and_compresses_whitespace() {
        let value = json!({
            "components": {"a": 1, "b": [2, 3]},
            "outer": {"z": null, "a": true},
        });
        let bytes = canonical_json_value(&value).unwrap();
        assert_eq!(
            bytes,
            br#"{"components":{"a":1,"b":[2,3]},"outer":{"a":true,"z":null}}"#.to_vec()
        );
    }

    /// RFC 8785 §3.2.2.2's escaping cases, on the characters that matter here: the five
    /// short forms, quote, backslash, and an exotic C0 control as `\u00xx`.
    #[test]
    fn string_escaping_matches_the_short_forms() {
        let value = json!("\u{0008}\u{0009}\u{000A}\u{000C}\u{000D}\"\\\u{0001}");
        let bytes = canonical_json_value(&value).unwrap();
        assert_eq!(bytes, br#""\b\t\n\f\r\"\\\u0001""#.to_vec());
    }

    /// Non-ASCII is UTF-8, never escaped (RFC 8785's é/本/emoji example).
    #[test]
    fn non_ascii_is_verbatim_utf8() {
        let value = json!("é 本 \u{1F916}");
        let bytes = canonical_json_value(&value).unwrap();
        assert_eq!(bytes, "\"é 本 \u{1F916}\"".as_bytes().to_vec());
    }

    /// A float anywhere in the tree is refused, typed with the crate's code.
    #[test]
    fn floats_are_refused() {
        let value = json!({"amount": 1.5});
        let error = canonical_json_value(&value).unwrap_err();
        assert_eq!(error.code(), "cmd.not_canonical");
        let nested = json!({"a": [{"deep": [0.0]}]});
        let error = canonical_json_value(&nested).unwrap_err();
        assert_eq!(error.code(), "cmd.not_canonical");
        // Even a float that spells like an integer has no canonical form.
        let integral_float = json!(2.0);
        let error = canonical_json_value(&integral_float).unwrap_err();
        assert_eq!(error.code(), "cmd.not_canonical");
    }

    /// Integers render as plain digits, negative and at the range edges.
    #[test]
    fn integers_are_plain_digits() {
        for (value, expected) in [
            (json!(0), "0"),
            (json!(1_000_000_000), "1000000000"),
            (json!(-7), "-7"),
            (json!(i64::MAX), "9223372036854775807"),
            (json!(i64::MIN), "-9223372036854775808"),
            (json!(u64::MAX), "18446744073709551615"),
        ] {
            assert_eq!(
                canonical_json_value(&value).unwrap(),
                expected.as_bytes().to_vec(),
                "{expected}"
            );
        }
    }

    /// Empty containers, deep key sorting, and the bare tokens.
    #[test]
    fn shapes_and_tokens() {
        assert_eq!(canonical_json_value(&json!({})).unwrap(), b"{}".to_vec());
        assert_eq!(canonical_json_value(&json!([])).unwrap(), b"[]".to_vec());
        assert_eq!(
            canonical_json_value(&json!(null)).unwrap(),
            b"null".to_vec()
        );
        let value = json!({"b": {"d": 1, "c": 2}, "a": [{"y": 1, "x": [{"k": 1, "j": 2}]}]});
        assert_eq!(
            canonical_json_value(&value).unwrap(),
            br#"{"a":[{"x":[{"j":2,"k":1}],"y":1}],"b":{"c":2,"d":1}}"#.to_vec()
        );
    }
}
