//! The scanner of the secret canary sweep (WP-SEC-03.T15).
//!
//! A [`Canary`] is one secret whose value only the test process knows. [`scan`] searches an
//! artefact (a backup, a log, a response body, a file) for the canary in the plain form and in
//! the encodings an accidental leak takes: base64 (standard and URL-safe, at all three byte
//! alignments), hex, URL-percent-encoding, JSON escaping, and changed case or reversed text.
//!
//! A [`Hit`] names the canary's label, the artefact and the encoding. It never carries the
//! value, and neither does the `Debug` form of a canary: a failing sweep prints labels only.

use std::fmt;

/// The fixed seed of the generated markers, so a failure reproduces.
const SEED: u64 = 0x5EC0_CA9A_17E5_7001;

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// One secret and every byte form a leak of it can take.
#[derive(Clone)]
pub struct Canary {
    label: String,
    value: String,
    forms: Vec<(&'static str, Vec<u8>)>,
}

impl fmt::Debug for Canary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Canary({})", self.label)
    }
}

impl Canary {
    /// A distinct, high-entropy-looking marker for `label` (`CANARY-<label>-<24 hex digits>`).
    /// The digits come from the label and a fixed seed: the same label always makes the same
    /// marker, and two labels never do.
    pub fn new(label: &str) -> Canary {
        let mut state = SEED;
        for byte in label.bytes() {
            state ^= u64::from(byte);
            state = state.wrapping_mul(0x0000_0100_0000_01B3);
        }
        let first = splitmix(&mut state);
        let second = splitmix(&mut state);
        let digits = format!("{first:016x}{second:016x}");
        Canary::from_value(label, &format!("CANARY-{label}-{}", &digits[..24]))
    }

    /// A secret the platform itself made (a token it returned, a key it issued): the same
    /// scanning, with the real value.
    pub fn from_value(label: &str, value: &str) -> Canary {
        Canary {
            label: label.to_owned(),
            value: value.to_owned(),
            forms: forms_of(value),
        }
    }

    /// The label: what a report may print.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The value, for planting it. Never print it.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Every searched form as `(name, bytes)`, the plain one first.
    pub fn forms(&self) -> &[(&'static str, Vec<u8>)] {
        &self.forms
    }
}

/// One canary found in one artefact. Holds names only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The canary's label.
    pub label: String,
    /// The artefact it was found in.
    pub artefact: String,
    /// The form it was found in (`plain`, `base64_a1`, `hex_lower`, ...).
    pub form: &'static str,
}

impl fmt::Display for Hit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "canary `{}` found in `{}` ({})",
            self.label, self.artefact, self.form
        )
    }
}

/// Searches `haystack` (the bytes of `artefact`) for every canary in every form. One hit per
/// canary and form; none when the artefact is clean.
pub fn scan(artefact: &str, haystack: &[u8], canaries: &[Canary]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for canary in canaries {
        for (form, needle) in &canary.forms {
            if contains(haystack, needle) {
                hits.push(Hit {
                    label: canary.label.clone(),
                    artefact: artefact.to_owned(),
                    form,
                });
            }
        }
    }
    hits
}

/// Whether `needle` occurs in `haystack`. An empty needle never does (it would match anything).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return false;
    };
    let mut from = 0;
    while let Some(offset) = haystack[from..].iter().position(|&b| b == first) {
        let at = from + offset;
        if haystack.len() - at < needle.len() {
            return false;
        }
        if haystack[at + 1..at + needle.len()] == *rest {
            return true;
        }
        from = at + 1;
    }
    false
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Standard or URL-safe base64 of `bytes`, without padding.
fn base64(bytes: &[u8], alphabet: &[u8; 64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let word = (b0 << 16) | (b1 << 8) | b2;
        let groups = chunk.len() + 1;
        for i in 0..groups {
            let index = (word >> (18 - 6 * i)) & 0x3F;
            out.push(alphabet[index as usize]);
        }
    }
    out
}

/// The base64 text a canary leaves inside a longer encoded stream when it starts `lead` bytes
/// after a three-byte boundary: only the characters that depend on the canary's bytes alone.
fn base64_at(value: &[u8], lead: usize, alphabet: &[u8; 64]) -> Vec<u8> {
    let mut buffer = vec![0u8; lead];
    buffer.extend_from_slice(value);
    let encoded = base64(&buffer, alphabet);
    let start = (8 * lead).div_ceil(6);
    let end = (8 * buffer.len()) / 6;
    if start >= end {
        return Vec::new();
    }
    encoded[start..end].to_vec()
}

fn hex(bytes: &[u8], upper: bool) -> Vec<u8> {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        if upper {
            out.push_str(&format!("{b:02X}"));
        } else {
            out.push_str(&format!("{b:02x}"));
        }
    }
    out.into_bytes()
}

/// Every byte as `%XX`: what a form encoder makes of a value it escapes in full.
fn percent(bytes: &[u8], upper: bool) -> Vec<u8> {
    let mut out = String::with_capacity(bytes.len() * 3);
    for b in bytes {
        if upper {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out.into_bytes()
}

/// Every character as `\uXXXX`: what a JSON writer makes of a value it escapes in full.
fn json_unicode(value: &str, upper: bool) -> Vec<u8> {
    let mut out = String::new();
    for unit in value.encode_utf16() {
        if upper {
            out.push_str(&format!("\\u{unit:04X}"));
        } else {
            out.push_str(&format!("\\u{unit:04x}"));
        }
    }
    out.into_bytes()
}

fn swap_case(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

fn forms_of(value: &str) -> Vec<(&'static str, Vec<u8>)> {
    let bytes = value.as_bytes();
    let json_plain = serde_json::to_string(value)
        .map(|s| s.trim_matches('"').as_bytes().to_vec())
        .unwrap_or_default();
    let candidates: Vec<(&'static str, Vec<u8>)> = vec![
        ("plain", bytes.to_vec()),
        ("lowercase", value.to_ascii_lowercase().into_bytes()),
        ("uppercase", value.to_ascii_uppercase().into_bytes()),
        ("swapped_case", swap_case(value).into_bytes()),
        ("reversed", bytes.iter().rev().copied().collect()),
        ("json_escaped", json_plain),
        ("json_unicode_lower", json_unicode(value, false)),
        ("json_unicode_upper", json_unicode(value, true)),
        ("base64_a0", base64_at(bytes, 0, STANDARD)),
        ("base64_a1", base64_at(bytes, 1, STANDARD)),
        ("base64_a2", base64_at(bytes, 2, STANDARD)),
        ("base64url_a0", base64_at(bytes, 0, URL_SAFE)),
        ("base64url_a1", base64_at(bytes, 1, URL_SAFE)),
        ("base64url_a2", base64_at(bytes, 2, URL_SAFE)),
        ("hex_lower", hex(bytes, false)),
        ("hex_upper", hex(bytes, true)),
        ("percent_lower", percent(bytes, false)),
        ("percent_upper", percent(bytes, true)),
    ];
    // A form that equals an earlier one (a value with nothing to escape or to re-case) is
    // searched once, under the first name; an empty form is never searched.
    let mut forms: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for (name, form) in candidates {
        if !form.is_empty() && !forms.iter().any(|(_, seen)| *seen == form) {
            forms.push((name, form));
        }
    }
    forms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_published_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes(), STANDARD), encoded.as_bytes());
        }
        // The two alphabets differ at the last two characters only.
        assert_eq!(base64(&[0xFB, 0xFF], STANDARD), b"+/8".to_vec());
        assert_eq!(base64(&[0xFB, 0xFF], URL_SAFE), b"-_8".to_vec());
    }

    #[test]
    fn a_marker_is_stable_distinct_and_labelled() {
        let a = Canary::new("api_key");
        assert_eq!(a.value(), Canary::new("api_key").value());
        assert_ne!(a.value(), Canary::new("password").value());
        assert!(a.value().starts_with("CANARY-api_key-"));
        assert_eq!(a.value().len(), "CANARY-api_key-".len() + 24);
        assert_eq!(a.label(), "api_key");
    }

    #[test]
    fn neither_debug_nor_a_hit_prints_the_value() {
        let canary = Canary::new("webhook");
        let hits = scan(
            "log",
            canary.value().as_bytes(),
            std::slice::from_ref(&canary),
        );
        assert!(!hits.is_empty());
        let shown = format!("{canary:?} {}", hits[0]);
        assert!(!shown.contains(canary.value()), "{shown}");
        assert!(
            shown.contains("webhook") && shown.contains("log"),
            "{shown}"
        );
    }

    /// The negative control of the harness: a canary planted in a fake artefact in each
    /// encoding is found, so a broken scanner cannot let the sweep pass.
    #[test]
    fn every_encoding_of_a_planted_canary_is_found() {
        let canary = Canary::new("secret_field");
        let names: Vec<&str> = canary.forms().iter().map(|(n, _)| *n).collect();
        for expected in [
            "plain",
            "lowercase",
            "swapped_case",
            "reversed",
            "json_unicode_lower",
            "json_unicode_upper",
            "base64_a0",
            "base64_a1",
            "base64_a2",
            "hex_lower",
            "hex_upper",
            "percent_lower",
            "percent_upper",
        ] {
            assert!(
                names.contains(&expected),
                "no `{expected}` form in {names:?}"
            );
        }
        for (name, form) in canary.forms() {
            let mut artefact = b"noise before ".to_vec();
            artefact.extend_from_slice(form);
            artefact.extend_from_slice(b" noise after");
            let hits = scan("fake", &artefact, std::slice::from_ref(&canary));
            assert!(
                hits.iter()
                    .any(|h| h.form == *name && h.label == "secret_field"),
                "the `{name}` form is not found"
            );
        }
    }

    /// The canary inside a larger base64 stream, at each of the three alignments.
    #[test]
    fn base64_is_found_at_every_alignment() {
        let canary = Canary::new("scim_credential");
        for lead in 0..3usize {
            for tail in 0..3usize {
                let mut raw = vec![b'x'; lead];
                raw.extend_from_slice(canary.value().as_bytes());
                raw.extend(std::iter::repeat_n(b'y', tail));
                for alphabet in [STANDARD, URL_SAFE] {
                    let mut text = b"{\"blob\":\"".to_vec();
                    text.extend(base64(&raw, alphabet));
                    text.extend_from_slice(b"\"}");
                    let hits = scan("blob", &text, std::slice::from_ref(&canary));
                    assert!(!hits.is_empty(), "lead {lead} tail {tail}");
                }
            }
        }
    }

    #[test]
    fn hex_percent_and_json_escapes_are_found_inside_text() {
        let canary = Canary::new("connector_credential");
        let value = canary.value();
        let encoded = [
            format!(
                "x={}&",
                String::from_utf8(hex(value.as_bytes(), false)).unwrap()
            ),
            format!(
                "x={}&",
                String::from_utf8(percent(value.as_bytes(), true)).unwrap()
            ),
            format!(
                "{{\"k\":\"{}\"}}",
                String::from_utf8(json_unicode(value, false)).unwrap()
            ),
            value.to_ascii_uppercase(),
            swap_case(value),
        ];
        for text in encoded {
            assert!(
                !scan("text", text.as_bytes(), std::slice::from_ref(&canary)).is_empty(),
                "not found in {} bytes of text",
                text.len()
            );
        }
    }

    #[test]
    fn a_clean_artefact_and_other_canaries_do_not_hit() {
        let a = Canary::new("password");
        let b = Canary::new("webhook_secret");
        assert!(scan("clean", b"nothing to see here", &[a.clone(), b.clone()]).is_empty());
        assert!(scan("empty", b"", std::slice::from_ref(&a)).is_empty());
        // A canary's text is not another canary's.
        assert!(scan("other", a.value().as_bytes(), std::slice::from_ref(&b)).is_empty());
        // Half of a marker is not the marker.
        let half = &a.value().as_bytes()[..a.value().len() - 1];
        assert!(scan("half", half, std::slice::from_ref(&a)).is_empty());
    }

    #[test]
    fn a_value_the_platform_made_is_scanned_like_a_marker() {
        let token = Canary::from_value("session_token", "spk_abc123_s3cr3t/with+chars");
        let text = format!("Authorization: Bearer {}", token.value());
        let hits = scan("headers", text.as_bytes(), std::slice::from_ref(&token));
        assert_eq!(
            hits.first().map(|h| h.label.as_str()),
            Some("session_token")
        );
    }
}
