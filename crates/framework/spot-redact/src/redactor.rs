//! The redactor: the [`Redactor`] every exit point calls.

use std::borrow::Cow;
use std::error::Error;

use serde_json::Value;

use crate::error::RedactError;
use crate::known::KnownSecrets;
use crate::patterns::Rules;
use crate::policy::{DEFAULT_FIELD_NAMES, RedactionPolicy};

/// The redactor: a compiled [`Rules`] set, a [`KnownSecrets`] registry and the JSON field
/// names. Share it through [`std::sync::Arc`] where it must be borrowed from several places
/// (the `tracing` writer does); known secrets are not `Clone`.
pub struct Redactor {
    known: KnownSecrets,
    rules: Rules,
    field_names: Vec<String>,
}

impl Redactor {
    /// Builds a redactor from the policy. No rule fails in practice: the patterns are
    /// compile-time strings, and a bug there surfaces as [`RedactError::Policy`].
    ///
    /// # Errors
    /// [`RedactError::Policy`] when a pattern rule fails to compile.
    pub fn from_policy(policy: &RedactionPolicy) -> Result<Self, RedactError> {
        Ok(Self {
            known: KnownSecrets::default(),
            rules: Rules::build(&policy.patterns)?,
            field_names: Self::field_names(&policy.field_names),
        })
    }

    /// The built-in sensitive JSON keys plus the policy's extras (case-insensitive).
    fn field_names(extras: &[String]) -> Vec<String> {
        let mut names: Vec<String> = DEFAULT_FIELD_NAMES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for extra in extras {
            let lower = extra.to_ascii_lowercase();
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&lower)) {
                names.push(lower);
            }
        }
        names
    }

    /// Registers a known secret under `label`; every occurrence of the value, and of its
    /// base64 and URL-encoded forms, becomes `[redacted:<label>]`.
    ///
    /// # Errors
    /// [`RedactError::BadLabel`] or [`RedactError::SecretTooShort`] (lengths in the error,
    /// never the value).
    pub fn register_secret(
        &mut self,
        label: &str,
        secret: &spot_secrets::SecretString,
    ) -> Result<(), RedactError> {
        self.known.register(label, secret)
    }

    /// How many known secrets are registered.
    #[must_use]
    pub fn known_secret_count(&self) -> usize {
        self.known.count()
    }

    /// The salted SHA-256 of a registered secret, for safe assertions and health pages.
    #[must_use]
    pub fn known_digest(&self, label: &str) -> Option<[u8; 32]> {
        self.known.digest_of(label)
    }
}

impl Redactor {
    /// Redacts one piece of text: known-secret forms first, then the pattern rules.
    /// Borrowed when nothing matched. Input over [`crate::MAX_INPUT_BYTES`] is truncated to
    /// the cap and the result carries [`crate::TRUNCATION_MARKER`].
    #[must_use]
    pub fn redact_str<'a>(&self, input: &'a str) -> Cow<'a, str> {
        self.redact_str_counted(input).0
    }

    /// [`redact_str`](Self::redact_str) plus the number of replacements made: what the
    /// bounded-growth property leans on.
    #[must_use]
    pub fn redact_str_counted<'a>(&self, input: &'a str) -> (Cow<'a, str>, usize) {
        // Cap first: a marker holds no trigger characters, and the truncated text is what
        // every rule then sees.
        let truncated = input.len() > crate::MAX_INPUT_BYTES;
        let mut text = if truncated {
            let mut cut = crate::MAX_INPUT_BYTES;
            while !input.is_char_boundary(cut) {
                cut -= 1;
            }
            input[..cut].to_owned()
        } else {
            String::from(input)
        };
        let mut count = self.apply_known(&mut text);
        count += self.rules.apply_all(&mut text);
        if count == 0 && !truncated {
            return (Cow::Borrowed(input), 0);
        }
        if truncated {
            text.push('\n');
            text.push_str(crate::TRUNCATION_MARKER);
        }
        (Cow::Owned(text), count)
    }

    /// Known secrets do not go through the regex table: their needles are looked up
    /// directly and spliced out left to right.
    fn apply_known(&self, text: &mut String) -> usize {
        let hits = self.known.matches(text);
        if hits.is_empty() {
            return 0;
        }
        let mut out = String::with_capacity(text.len() + 16);
        let mut start = 0usize;
        for (range, label) in &hits {
            out.push_str(&text[start..range.start]);
            out.push_str(crate::MARKER_OPEN);
            out.push_str(label);
            out.push_str(crate::MARKER_CLOSE);
            start = range.end;
        }
        out.push_str(&text[start..]);
        *text = out;
        hits.len()
    }

    /// Redacts a JSON value in place: values of sensitive keys (the field names,
    /// case-insensitively) become `[redacted]`, and every string value goes through
    /// [`redact_str`](Self::redact_str).
    pub fn redact_json(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if self.field_names.iter().any(|n| n.eq_ignore_ascii_case(key)) {
                        *child = Value::String("[redacted]".to_owned());
                    } else {
                        self.redact_json(child);
                    }
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    self.redact_json(item);
                }
            }
            Value::String(text) => {
                let redacted = self.redact_str(text);
                if let Cow::Owned(new) = redacted {
                    *text = new;
                }
            }
            _ => {}
        }
    }

    /// Redacts an error's whole chain (its `Display` and every `source`) into one string,
    /// joined with `: `.
    #[must_use]
    pub fn redact_error_chain(&self, error: &(dyn Error + 'static)) -> String {
        let mut text = error.to_string();
        let mut source = error.source();
        while let Some(e) = source {
            text.push_str(": ");
            text.push_str(&e.to_string());
            source = e.source();
        }
        self.redact_str(&text).into_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // the brief exempts tests from the no-expect rule
    use super::*;
    use crate::policy::RedactionPolicy;
    use serde_json::json;
    use std::error::Error;

    fn redactor() -> Redactor {
        Redactor::from_policy(&RedactionPolicy::default()).expect("the default policy compiles")
    }

    #[test]
    fn borrowed_when_nothing_matched() {
        let r = redactor();
        let text = "an ordinary log line";
        assert!(matches!(r.redact_str(text), Cow::Borrowed(_)));
        let out = r.redact_str("Authorization: SkjAjN3jM2kVwP9qLb");
        assert!(matches!(out, Cow::Owned(_)));
    }

    #[test]
    fn json_walks_keys_values_and_arrays() {
        let r = redactor();
        let mut v = json!({
            "user": "jane",
            "password": "hunter 2",
            "nested": {"api_key": "sk-123", "list": ["Bearer qW12er45Ty78", "token-value-1234567", 5]},
            "note": "Authorization: SkjAjN3jM2kVwP9qLb"
        });
        r.redact_json(&mut v);
        assert_eq!(v["password"], json!("[redacted]"));
        assert_eq!(v["nested"]["api_key"], json!("[redacted]"));
        // The bearer rule fires on the scheme and keeps it; absent key context and field
        // names nothing else would fire.
        assert_eq!(v["nested"]["list"][0], json!("Bearer [redacted:token]"));
        // No rule matches a bare word: redaction is selective, not blanket.
        assert_eq!(v["nested"]["list"][1], json!("token-value-1234567"));
        assert_eq!(v["nested"]["list"][2], json!(5));
        assert_eq!(v["note"], json!("Authorization: [redacted:header]"));
        assert_eq!(v["user"], json!("jane"));
    }

    #[test]
    fn json_is_idempotent() {
        let r = redactor();
        let mut v = json!({"password": "hunter 2", "note": "Authorization: SkjAjN3jM2kVwP9qLb"});
        r.redact_json(&mut v);
        let first = v.clone();
        r.redact_json(&mut v);
        assert_eq!(v, first);
    }

    #[derive(Debug)]
    struct ChainError(&'static str);

    impl std::fmt::Display for ChainError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for ChainError {}

    #[test]
    fn error_chain_is_joined_and_redacted() {
        let r = redactor();
        let inner: Box<dyn Error> = Box::new(ChainError(
            "query failed: Authorization: SkjAjN3jM2kVwP9qLb",
        ));
        let outer = ChainErrorWithSource("handler died", inner);
        let out = r.redact_error_chain(&outer);
        assert_eq!(
            out,
            "handler died: query failed: Authorization: [redacted:header]"
        );
    }

    #[derive(Debug)]
    struct ChainErrorWithSource(&'static str, Box<dyn Error>);

    impl std::fmt::Display for ChainErrorWithSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for ChainErrorWithSource {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.1.as_ref())
        }
    }

    #[test]
    fn counted_replacements_match_the_marker_count() {
        let r = redactor();
        // The comma ends the header value, so the spk_ key is a separate match.
        let text = "Authorization: SkjAjN3jM2kVwP9qLb, and spk_abcdefgh2345_PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA_2O7";
        let (out, count) = r.redact_str_counted(text);
        assert_eq!(count, 2);
        assert_eq!(
            out,
            "Authorization: [redacted:header], and [redacted:api_key]"
        );
    }

    #[test]
    fn truncation_caps_and_marks() {
        let r = redactor();
        let mut big = String::with_capacity(1_600_000);
        for _ in 0..80_000 {
            big.push_str("Authorization: SkjAjN3jM2kVwP9qLb\n");
        }
        assert!(big.len() > crate::MAX_INPUT_BYTES);
        let out = r.redact_str(&big).into_owned();
        assert!(out.ends_with(crate::TRUNCATION_MARKER));
        assert!(out.len() < crate::MAX_INPUT_BYTES + 64);
        // Idempotence holds across the cap.
        let second = r.redact_str(&out).into_owned();
        assert_eq!(second, out);
    }
}
