//! The known-secret registry: values registered at runtime, never read from the policy.
//!
//! Registering a secret with [`KnownSecrets::register`] keeps four things:
//!
//! 1. the secret's **label** (public, also the marker text),
//! 2. a **salted SHA-256 digest plus length** — what survives for reporting; nothing that
//!    reads the registry prints a secret,
//! 3. in [`Zeroizing`] memory, the **matching forms**: the value itself, its base64
//!    encodings and its URL-encoded form,
//! 4. nothing else, and all of it is wiped when the registry drops.
//!
//! Form matching needs the value in memory: finding "any substring or encoding of the
//! secret" by digest alone would mean hashing every substring suffix of the input, which is
//! impractical; this is the documented trade-off (crate docs).
//!
//! Matching is exact for the value itself (a substring of the input), and covers the
//! standard and URL-safe base64 encodings (padded and unpadded) and URL-encoded forms
//! (upper- and lower-case hex escapes).

use std::ops::Range;

use zeroize::Zeroizing;

use crate::encoding::percent_encode;
use crate::error::RedactError;

/// Minimum length in bytes of a value the registry accepts. Anything shorter is unsafe to
/// match as a substring: two-letter words collide with ordinary text.
const MIN_SECRET_BYTES: usize = 8;

/// Maximum label length: keeps every replacement within the growth bound
/// (``[redacted:<label>]`` is `11 + label` bytes, and a minimum 8-byte secret admits
/// labels of up to 20 characters).
const MAX_LABEL_BYTES: usize = 20;

/// A label must be one to twenty characters of `a-z`, `0-9` or `_`.
fn label_is_well_formed(label: &str) -> bool {
    let len = label.len();
    !label.is_empty()
        && len <= MAX_LABEL_BYTES
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The matching forms of one registered secret: the needles the scanner looks for (the
/// value itself first), wiped when the registry drops.
struct Entry {
    label: String,
    /// Every needle searched for; the value first, then its base64 and URL encodings.
    needles: Vec<Zeroizing<String>>,
    /// Salted SHA-256 of the value: safe to print or store.
    digest: [u8; 32],
    /// The value's byte length: safe to print or store.
    len: usize,
}

/// All registered secrets: matching is longest-needle-first, and a label may be re-used by
/// registering again (for example after a token rotation).
#[derive(Default)]
pub struct KnownSecrets {
    entries: Vec<Entry>,
    salt_counter: u64,
}

/// Salted digest of one registered secret; also the identity a label maps to.
type Digest = [u8; 32];

impl KnownSecrets {
    /// Registers a secret under `label`. Every occurrence of the value (and of its base64
    /// and URL-encoded forms) is later replaced by `[redacted:<label>]`.
    pub fn register(
        &mut self,
        label: &str,
        secret: &spot_secrets::SecretString,
    ) -> Result<(), RedactError> {
        if !label_is_well_formed(label) {
            return Err(RedactError::BadLabel);
        }
        let value = secret.expose().to_owned();
        if value.len() < MIN_SECRET_BYTES {
            return Err(RedactError::SecretTooShort { len: value.len() });
        }
        // Monotonic counter salt mixed with wall-clock nanos: enough for uniqueness across
        // registrations in one process. The digest is for reporting, never key material.
        self.salt_counter = self.salt_counter.wrapping_add(1);
        let salt = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u64, |d| d.as_nanos() as u64)
            .wrapping_add(self.salt_counter);
        let len = value.len();
        let digest = Self::digest_for(&value, salt);
        let needles = Entry::needles_from(&value);
        self.entries.retain(|e| e.label != label);
        self.entries.push(Entry {
            label: label.to_owned(),
            needles,
            digest,
            len,
        });
        // Longest values first: a superstring wins over a substring at the same offset.
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.len));
        Ok(())
    }

    /// How many secrets are registered (never their names or values).
    #[must_use]
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// The salted digest of a registered label, for tests and health reporting that must not
    /// print the secret itself.
    #[must_use]
    pub fn digest_of(&self, label: &str) -> Option<Digest> {
        self.entries
            .iter()
            .find(|e| e.label == label)
            .map(|e| e.digest)
    }

    /// The registered value's byte length for a label.
    #[must_use]
    pub fn len_of(&self, label: &str) -> Option<usize> {
        self.entries
            .iter()
            .find(|e| e.label == label)
            .map(|e| e.len)
    }

    /// Finds every known-secret occurrence in `text` and returns `(range, label)` pairs,
    /// left to right, non-overlapping, longest needle first at any offset.
    #[must_use]
    pub fn matches(&self, text: &str) -> Vec<(Range<usize>, String)> {
        let lower = text.to_ascii_lowercase();
        let mut found: Vec<(Range<usize>, String)> = Vec::new();
        for entry in &self.entries {
            for needle in &entry.needles {
                // Percent-encoded needles match both hex cases; the raw and base64 forms are
                // exact. A needle is ASCII, so lowering preserves byte offsets.
                let ci = needle.contains('%');
                Self::collect(text, &lower, needle, &entry.label, ci, &mut found);
            }
        }
        found.sort_by_key(|(r, _)| (r.start, usize::MAX - r.end));
        let mut kept: Vec<(Range<usize>, String)> = Vec::new();
        let mut end = 0usize;
        for (r, label) in found {
            if r.start >= end {
                end = r.end;
                kept.push((r, label));
            }
        }
        kept
    }

    /// Collects non-overlapping matches of `needle` in `text`, case-insensitively against
    /// the pre-lowered copy when asked.
    fn collect(
        text: &str,
        lower: &str,
        needle: &str,
        label: &str,
        ci: bool,
        found: &mut Vec<(Range<usize>, String)>,
    ) {
        let hay: &str = if ci { lower } else { text };
        let needle_lc = ci.then(|| needle.to_ascii_lowercase());
        let needle: &str = needle_lc.as_deref().unwrap_or(needle);
        let mut start = 0usize;
        while let Some(rel) = hay[start..].find(needle) {
            let at = start + rel;
            found.push((at..at + needle.len(), label.to_owned()));
            start = at + needle.len();
            if start >= hay.len() {
                break;
            }
        }
    }

    /// Salts and digests the value.
    fn digest_for(value: &str, salt: u64) -> Digest {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(salt.to_le_bytes());
        hasher.update(value.as_bytes());
        hasher.finalize().into()
    }
}

impl Entry {
    /// Builds the matching forms of the value: the value itself, its four base64 paddings
    /// (standard and URL-safe alphabets) and its percent-encoded form.
    fn needles_from(value: &str) -> Vec<Zeroizing<String>> {
        let mut parts = vec![Zeroizing::new(value.to_owned())];
        let push = |form: String, parts: &mut Vec<Zeroizing<String>>| {
            if !parts.iter().any(|p| **p == form) {
                parts.push(Zeroizing::new(form));
            }
        };
        use base64::Engine;
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };
        let bytes = value.as_bytes();
        for engine in [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD] {
            push(engine.encode(bytes), &mut parts);
        }
        push(percent_encode(bytes), &mut parts);
        parts
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // the brief exempts tests from the no-expect rule
    use super::{KnownSecrets, label_is_well_formed};
    use crate::encoding::percent_encode;
    use crate::error::RedactError;
    use spot_secrets::SecretString;

    fn register(secrets: &mut KnownSecrets, label: &str, value: &str) {
        secrets
            .register(label, &SecretString::new(value))
            .expect("the test values are well formed");
    }

    #[test]
    fn short_secrets_are_rejected() {
        let mut s = KnownSecrets::default();
        let short = "hunter2";
        assert_eq!(
            s.register("x", &SecretString::new(short)),
            Err(RedactError::SecretTooShort { len: short.len() })
        );
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn labels_are_constrained() {
        assert!(label_is_well_formed("staging_db"));
        assert!(!label_is_well_formed(""));
        assert!(!label_is_well_formed(&"a".repeat(21)));
        assert!(!label_is_well_formed("Has Dots.No"));
        assert!(!label_is_well_formed("UPPER"));
    }

    #[test]
    fn raw_and_encoded_forms_all_match() {
        let mut s = KnownSecrets::default();
        register(&mut s, "staging_db", "hunter2staging");
        // Raw substring.
        let out = s.matches("login failed with hunter2staging from db pool");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "staging_db");
        // URL-encoded (upper and lower hex).
        let upper = percent_encode("hunter2staging".as_bytes());
        let lower = upper.to_ascii_lowercase();
        for form in [upper, lower] {
            let out = s.matches(&format!("pw={form}"));
            assert_eq!(out.len(), 1, "the URL-encoded form matches");
        }
        // Base64, both alphabets and paddings, via the same needle set.
        use base64::Engine;
        use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
        let std = STANDARD.encode("hunter2staging".as_bytes());
        let url = URL_SAFE_NO_PAD.encode("hunter2staging".as_bytes());
        for form in [std, url] {
            let out = s.matches(&format!("blob={form}"));
            assert_eq!(out.len(), 1, "the base64 form matches");
        }
    }

    #[test]
    fn re_registering_a_label_replaces_it() {
        let mut s = KnownSecrets::default();
        register(&mut s, "k", "aaaaaaaaaaaaaaaa");
        let d1 = s.digest_of("k");
        register(&mut s, "k", "bbbbbbbbbbbbbbbbbb");
        assert_eq!(s.count(), 1);
        assert_ne!(s.digest_of("k"), d1);
        assert_eq!(s.len_of("k"), Some(18));
    }

    #[test]
    fn longer_entries_win_over_substrings() {
        let mut s = KnownSecrets::default();
        register(&mut s, "short", "abcdefgh");
        register(&mut s, "long", "abcdefghij");
        let out = s.matches("abcdefghij is the value");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "long");
    }

    #[test]
    fn percent_encode_leaves_unreserved_bytes() {
        assert_eq!(
            percent_encode("a-b_c.d~e f/g".as_bytes()),
            "a-b_c.d~e%20f%2Fg"
        );
    }
}
