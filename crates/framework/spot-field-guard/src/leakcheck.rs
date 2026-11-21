//! The leak harness (ADR-070 section 12, rule 6): one test shape that proves a channel does
//! not leak, used by this crate's matrix and by every later task that adds a channel.
//!
//! Plant the secret-looking values of a [`Corpus`] in protected fields, run the channel,
//! serialise what leaves it (JSON, CSV, text or bytes) and hand the outputs to
//! [`assert_no_leak`]. It fails if **any** corpus value, or a trivial encoding of it, appears
//! anywhere in any output. The failure names the secret's label and the encoding, **never the
//! value**.
//!
//! Trivial encodings searched: the raw text; the separators-stripped form (`4111111111111111`
//! for `4111 1111 1111 1111`); upper and lower case (the search ignores ASCII case); the
//! lookalike-folded form (fullwidth digits and letters, Cyrillic and Greek lookalikes, and
//! zero-width characters are folded to plain ASCII on both sides); base64 at all three byte
//! alignments, standard and URL alphabets, so a value embedded in a larger base64 blob is
//! found; hex; percent-encoding; and the JSON-escaped form.

use spot_permissions::{MaskFormat, mask};

/// One secret-looking value (or one split across several fields) with its label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Secret {
    /// A name for reports: the label is safe to print, the value is not.
    pub label: String,
    /// What the test plants in fields: one string, or the parts of a value split across
    /// several fields or child rows.
    pub planted: Vec<String>,
    /// Further strings that must not appear: the secret inside a container (a JSON text
    /// that the field holds).
    pub inner: Vec<String>,
}

impl Secret {
    /// A value planted in one field.
    #[must_use]
    pub fn whole(label: &str, value: &str) -> Secret {
        Secret {
            label: label.to_owned(),
            planted: vec![value.to_owned()],
            inner: Vec::new(),
        }
    }

    /// A secret inside a container value: the field holds `container` (a JSON text, say),
    /// and `secret` inside it must not appear anywhere either, in any encoding.
    #[must_use]
    pub fn embedded(label: &str, container: &str, secret: &str) -> Secret {
        Secret {
            label: label.to_owned(),
            planted: vec![container.to_owned()],
            inner: vec![secret.to_owned()],
        }
    }

    /// A value split into parts planted in different fields or rows. Each part, and the
    /// parts joined, are searched for.
    #[must_use]
    pub fn split(label: &str, parts: &[&str]) -> Secret {
        Secret {
            label: label.to_owned(),
            planted: parts.iter().map(|p| (*p).to_owned()).collect(),
            inner: Vec::new(),
        }
    }

    /// The first planted string (the whole value for an unsplit secret).
    #[must_use]
    pub fn value(&self) -> &str {
        self.planted.first().map_or("", String::as_str)
    }

    /// The strings that must not appear: every part, and (for a split) the parts joined with
    /// no separator and with a space.
    fn needles(&self) -> Vec<String> {
        let mut out: Vec<String> = self.planted.clone();
        out.extend(self.inner.iter().cloned());
        if self.planted.len() > 1 {
            out.push(self.planted.concat());
            out.push(self.planted.join(" "));
        }
        out.retain(|n| n.chars().count() >= 4);
        out
    }
}

/// The secrets a channel is tried against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Corpus {
    /// The secrets, in a stable order.
    pub secrets: Vec<Secret>,
}

impl Corpus {
    /// An empty corpus.
    #[must_use]
    pub fn new() -> Corpus {
        Corpus::default()
    }

    /// Adds a secret.
    #[must_use]
    pub fn with(mut self, secret: Secret) -> Corpus {
        self.secrets.push(secret);
        self
    }

    /// The standard corpus: a card-like number, an IBAN, a salary figure, a value with
    /// unicode lookalikes (Cyrillic letters standing in for Latin), a value inside a JSON
    /// string, and a value split across child rows.
    #[must_use]
    pub fn standard() -> Corpus {
        Corpus::new()
            .with(Secret::whole("card", "4111 1111 1111 1111"))
            .with(Secret::whole("iban", "GB82WEST12345698765432"))
            .with(Secret::whole("salary", "84500.75"))
            .with(Secret::whole(
                "lookalike",
                "\u{406}\u{412}AN-7781-\u{421}\u{421}99",
            ))
            .with(Secret::embedded(
                "in_json",
                r#"{"pan":"5500 0000 0000 0004","note":"keep"}"#,
                "5500 0000 0000 0004",
            ))
            .with(Secret::split("split_rows", &["4929 1234", "5678 9012"]))
    }

    /// The secrets with these labels only (a principal that may see some of them).
    #[must_use]
    pub fn only(&self, labels: &[&str]) -> Corpus {
        Corpus {
            secrets: self
                .secrets
                .iter()
                .filter(|s| labels.contains(&s.label.as_str()))
                .cloned()
                .collect(),
        }
    }

    /// The corpus without the secrets with these labels.
    #[must_use]
    pub fn without(&self, labels: &[&str]) -> Corpus {
        Corpus {
            secrets: self
                .secrets
                .iter()
                .filter(|s| !labels.contains(&s.label.as_str()))
                .cloned()
                .collect(),
        }
    }

    /// One secret by label.
    #[must_use]
    pub fn get(&self, label: &str) -> Option<&Secret> {
        self.secrets.iter().find(|s| s.label == label)
    }
}

/// What a channel produced, in the form it leaves in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// A JSON text.
    Json(String),
    /// A CSV text.
    Csv(String),
    /// Plain text (a notification body, a print page, a log line).
    Text(String),
    /// Raw bytes (a file, a wire frame).
    Bytes(Vec<u8>),
}

impl Output {
    fn bytes(&self) -> &[u8] {
        match self {
            Output::Json(t) | Output::Csv(t) | Output::Text(t) => t.as_bytes(),
            Output::Bytes(b) => b,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Output::Json(_) => "json",
            Output::Csv(_) => "csv",
            Output::Text(_) => "text",
            Output::Bytes(_) => "bytes",
        }
    }
}

/// One secret found in one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leak {
    /// The secret's label (never its value).
    pub secret: String,
    /// Which form matched: `raw`, `stripped`, `folded`, `base64`, `base64url`, `hex`,
    /// `percent` or `json_escaped`.
    pub encoding: &'static str,
    /// The index of the output in the slice given.
    pub output: usize,
    /// The output's kind.
    pub kind: &'static str,
}

/// Maps a character to its plain ASCII look: fullwidth forms, Cyrillic and Greek lookalikes.
/// Zero-width characters map to `None` (dropped).
fn fold_char(c: char) -> Option<char> {
    Some(match c {
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}' => {
            return None;
        }
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(u32::from(c) - 0xFEE0).unwrap_or(c),
        '\u{0410}' | '\u{0391}' => 'A',
        '\u{0412}' | '\u{0392}' => 'B',
        '\u{0415}' | '\u{0395}' => 'E',
        '\u{041A}' | '\u{039A}' => 'K',
        '\u{041C}' | '\u{039C}' => 'M',
        '\u{041D}' | '\u{0397}' => 'H',
        '\u{041E}' | '\u{039F}' => 'O',
        '\u{0420}' | '\u{03A1}' => 'P',
        '\u{0421}' => 'C',
        '\u{0422}' | '\u{03A4}' => 'T',
        '\u{0425}' | '\u{03A7}' => 'X',
        '\u{0406}' | '\u{0399}' => 'I',
        '\u{0430}' => 'a',
        '\u{0435}' => 'e',
        '\u{043E}' | '\u{03BF}' => 'o',
        '\u{0440}' => 'p',
        '\u{0441}' => 'c',
        '\u{0445}' => 'x',
        '\u{0443}' => 'y',
        '\u{0456}' => 'i',
        other => other,
    })
}

fn fold(text: &str) -> String {
    text.chars().filter_map(fold_char).collect()
}

fn lower(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64 of `data` in `alphabet`.
fn base64(data: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let idx = ((n >> (18 - 6 * i)) & 0x3f) as usize;
            out.push(char::from(alphabet.get(idx).copied().unwrap_or(b'=')));
        }
    }
    out
}

/// The stable middle of the base64 of `needle` at one of the three byte alignments: the
/// characters that do not depend on the bytes around it.
fn base64_stable(needle: &[u8], alphabet: &[u8; 64], alignment: usize) -> String {
    let mut padded = vec![0u8; alignment];
    padded.extend_from_slice(needle);
    let encoded = base64(&padded, alphabet);
    let skip = [0usize, 2, 3].get(alignment).copied().unwrap_or(0);
    let mut chars: Vec<char> = encoded.chars().skip(skip).collect();
    if !padded.len().is_multiple_of(3) {
        chars.pop();
    }
    chars.into_iter().collect()
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn percent(data: &[u8]) -> String {
    let mut out = String::new();
    for &b in data {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

fn json_escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or("")
        .to_owned()
}

/// Every searched form of one needle: `(encoding, lowercased text)`.
fn variants(needle: &str) -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    let mut push = |encoding: &'static str, text: String| {
        if text.chars().count() >= 4 {
            out.push((encoding, lower(text.as_bytes())));
        }
    };
    push("raw", needle.to_owned());
    let stripped: String = needle.chars().filter(|c| c.is_alphanumeric()).collect();
    if stripped != needle {
        push("stripped", stripped);
    }
    let folded = fold(needle);
    if folded != needle {
        push("folded", folded.clone());
        let folded_stripped: String = folded.chars().filter(|c| c.is_alphanumeric()).collect();
        push("stripped", folded_stripped);
    }
    for form in [needle, folded.as_str()] {
        for alignment in 0..3 {
            push("base64", base64_stable(form.as_bytes(), B64_STD, alignment));
            push(
                "base64url",
                base64_stable(form.as_bytes(), B64_URL, alignment),
            );
        }
        push("hex", hex(form.as_bytes()));
        push("percent", percent(form.as_bytes()));
        push("json_escaped", json_escaped(form));
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Every corpus value (or trivial encoding of one) found in any output. Empty means clean.
#[must_use]
pub fn find_leaks(corpus: &Corpus, outputs: &[Output]) -> Vec<Leak> {
    let mut leaks = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        let raw = lower(output.bytes());
        let text = String::from_utf8_lossy(output.bytes());
        let folded = lower(fold(&text).as_bytes());
        for secret in &corpus.secrets {
            let mut found: Option<&'static str> = None;
            'search: for needle in secret.needles() {
                for (encoding, variant) in variants(&needle) {
                    if contains(&raw, &variant) || contains(&folded, &variant) {
                        found = Some(encoding);
                        break 'search;
                    }
                }
            }
            if let Some(encoding) = found {
                leaks.push(Leak {
                    secret: secret.label.clone(),
                    encoding,
                    output: index,
                    kind: output.kind(),
                });
            }
        }
    }
    leaks
}

/// Fails the test if any corpus value, or a trivial encoding of it, is in any output. The
/// panic message lists the secret labels, encodings and output indexes, never a value.
///
/// # Panics
///
/// On a leak.
pub fn assert_no_leak(corpus: &Corpus, outputs: &[Output]) {
    let leaks = find_leaks(corpus, outputs);
    assert!(
        leaks.is_empty(),
        "channel leaked {} secret(s): {}",
        leaks.len(),
        leaks
            .iter()
            .map(|l| format!(
                "{} as {} in output {} ({})",
                l.secret, l.encoding, l.output, l.kind
            ))
            .collect::<Vec<_>>()
            .join("; ")
    );
}

/// Checks that `masked` reveals no more of `original` than the permission core's `mask`
/// allows under `format`: no longer than the input, and every character that is not `*` is
/// the same character the core's mask shows at that position.
///
/// # Errors
///
/// A description naming the position (never a character of the value).
pub fn check_mask(original: &str, format: &MaskFormat, masked: &str) -> Result<(), String> {
    let allowed: Vec<char> = mask(original, format).chars().collect();
    let got: Vec<char> = masked.chars().collect();
    if got.len() > original.chars().count() {
        return Err("the masked form is longer than the value".to_owned());
    }
    if matches!(format, MaskFormat::Placeholder) && got.len() > 3 {
        return Err("a placeholder mask is longer than three characters".to_owned());
    }
    for (i, c) in got.iter().enumerate() {
        if *c != '*' && allowed.get(i) != Some(c) {
            return Err(format!("position {i} shows a character the mask hides"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Corpus {
        Corpus::standard()
    }

    #[test]
    fn clean_output_is_clean() {
        let out = [Output::Json(
            r#"{"number":"INV-1","card":"***********1111"}"#.into(),
        )];
        assert!(find_leaks(&corpus(), &out).is_empty());
    }

    #[test]
    fn every_trivial_encoding_is_found() {
        let card = "4111 1111 1111 1111";
        let forms: Vec<(&str, String)> = vec![
            ("raw", format!("x {card} y")),
            ("stripped", "pan=4111111111111111;".to_owned()),
            (
                "folded",
                "\u{FF14}\u{FF11}\u{FF11}\u{FF11} 1111 1111 1111".to_owned(),
            ),
            ("base64", base64(format!("v:{card}").as_bytes(), B64_STD)),
            ("base64", base64(format!("vv:{card}").as_bytes(), B64_STD)),
            ("base64", base64(format!("vvv:{card}").as_bytes(), B64_STD)),
            (
                "base64url",
                base64(format!("vv:{card}").as_bytes(), B64_URL),
            ),
            ("hex", hex(card.as_bytes())),
            ("percent", percent(card.as_bytes())),
            ("raw", card.to_ascii_uppercase()),
        ];
        for (name, text) in forms {
            let leaks = find_leaks(&corpus().only(&["card"]), &[Output::Text(text.clone())]);
            assert!(!leaks.is_empty(), "{name} form was not found");
        }
    }

    #[test]
    fn a_lookalike_value_is_found_after_folding() {
        let latin = "IBAN-7781-CC99";
        let leaks = find_leaks(
            &corpus().only(&["lookalike"]),
            &[Output::Text(latin.into())],
        );
        assert_eq!(leaks.len(), 1);
        let leaks = find_leaks(
            &corpus().only(&["lookalike"]),
            &[Output::Text("I\u{200B}BAN-7781-CC99".into())],
        );
        assert_eq!(
            leaks.len(),
            1,
            "a zero-width character must not hide a value"
        );
    }

    #[test]
    fn a_value_inside_a_json_string_is_found() {
        let inner = "5500 0000 0000 0004";
        let leaks = find_leaks(
            &corpus().only(&["in_json"]),
            &[Output::Json(format!(r#"{{"a":"{inner}"}}"#))],
        );
        assert_eq!(leaks.len(), 1);
        let escaped = json_escaped(r#"{"pan":"5500 0000 0000 0004","note":"keep"}"#);
        let leaks = find_leaks(
            &corpus().only(&["in_json"]),
            &[Output::Json(format!(r#"{{"blob":"{escaped}"}}"#))],
        );
        assert!(!leaks.is_empty());
    }

    #[test]
    fn one_part_of_a_split_value_is_a_leak() {
        let leaks = find_leaks(
            &corpus().only(&["split_rows"]),
            &[Output::Csv("items,4929 1234\n".into())],
        );
        assert_eq!(leaks.len(), 1);
    }

    #[test]
    fn bytes_are_searched_too() {
        let mut bytes = vec![0u8, 1, 2];
        bytes.extend_from_slice(b"84500.75");
        let leaks = find_leaks(&corpus(), &[Output::Bytes(bytes)]);
        assert_eq!(leaks[0].secret, "salary");
        assert_eq!(leaks[0].kind, "bytes");
    }

    #[test]
    fn a_failure_names_no_value() {
        let leaks = find_leaks(&corpus(), &[Output::Text("84500.75".into())]);
        let text = format!("{leaks:?}");
        assert!(!text.contains("84500"));
    }

    #[test]
    fn the_mask_check_bounds_what_a_mask_may_show() {
        let f = MaskFormat::LastChars(4);
        let ok = mask("GB82WEST12345698765432", &f);
        assert!(check_mask("GB82WEST12345698765432", &f, &ok).is_ok());
        assert!(check_mask("GB82WEST12345698765432", &f, "GB82WEST12345698765432").is_err());
        assert!(check_mask("abcd", &MaskFormat::Placeholder, "****").is_err());
        assert!(check_mask("abcd", &MaskFormat::Placeholder, "***").is_ok());
        assert!(check_mask("12", &MaskFormat::LastChars(4), "12").is_err());
        assert!(check_mask("12", &MaskFormat::LastChars(4), "*2").is_ok());
    }
}
