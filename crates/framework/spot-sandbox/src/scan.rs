//! The leak scanner: proves that no original value of a masked field survives anywhere in the
//! sandbox.
//!
//! # How it works
//!
//! While masking, every original value that was masked or stripped is reduced to two
//! normalised digests — the lower-cased whitespace-collapsed text and its punctuation-free
//! compact form — and inserted into a [`SourceIndex`]. The index holds a Bloom filter for a
//! fast negative answer and the exact digest set that confirms a hit, so a Bloom false positive
//! never quarantines a clean sandbox.
//!
//! [`scan`] reads every surface the sandbox has and looks for each indexed value exactly and as
//! a word inside a longer text. A [`Leak`] names the surface, the record, the field and a
//! digest prefix — **never the value** — so the report can be attached to a task or a log line.
//!
//! # What is not indexed
//!
//! Values shorter than [`SourceIndex::min_chars`] (four by default) are skipped. A three-letter
//! string cannot be distinguished from an ordinary word, and scanning for it would flag half
//! the sandbox. Amounts are kept by design and are not source values (see
//! [`crate::mask`]). The policy exposes the threshold.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::data::SandboxStore;

/// Where a leak was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// A document field.
    Documents,
    /// A journal row.
    Journal,
    /// An audit row (before or after).
    Audit,
    /// The search index.
    Search,
    /// Attachment metadata or content.
    Attachments,
    /// A cache entry (key or value).
    Caches,
}

impl Surface {
    /// The stable name the report uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Documents => "documents",
            Surface::Journal => "journal",
            Surface::Audit => "audit",
            Surface::Search => "search",
            Surface::Attachments => "attachments",
            Surface::Caches => "caches",
        }
    }
}

/// One unmasked original value, described without the value itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leak {
    /// The surface that still holds the value.
    pub surface: Surface,
    /// The DocType (or the surface name when it has none, such as a cache entry).
    pub doctype: String,
    /// The record id or cache key.
    pub record: String,
    /// The field that holds it.
    pub field: String,
    /// The first 16 hex characters of the matched normalised digest.
    pub digest: String,
}

/// What one scan read and found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    /// The tenant this store is bound to.
    pub tenant: String,
    /// Values read from the surfaces.
    pub scanned_values: usize,
    /// How many values the masker indexed.
    pub indexed_values: usize,
    /// Values read per surface.
    pub by_surface: BTreeMap<String, usize>,
    /// Every leak, in surface order.
    pub leaks: Vec<Leak>,
}

impl ScanReport {
    /// Whether the sandbox holds no original value of a masked field.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.leaks.is_empty()
    }

    /// The leaks found on one surface.
    #[must_use]
    pub fn leaks_on(&self, surface: Surface) -> Vec<&Leak> {
        self.leaks
            .iter()
            .filter(|leak| leak.surface == surface)
            .collect()
    }
}

/// A set of normalised source values: a Bloom filter first, the exact digests to confirm.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceIndex {
    exact: BTreeSet<[u8; 32]>,
    bloom: Bloom,
    capacity: usize,
    min_chars: usize,
}

impl SourceIndex {
    /// An index that skips values shorter than `min_chars`.
    #[must_use]
    pub fn new(min_chars: usize) -> Self {
        Self {
            exact: BTreeSet::new(),
            bloom: Bloom::with_capacity(0),
            capacity: 0,
            min_chars,
        }
    }

    /// Builds an index from values.
    #[must_use]
    pub fn from_values<'a>(values: impl IntoIterator<Item = &'a str>, min_chars: usize) -> Self {
        let mut index = Self::new(min_chars);
        for value in values {
            index.add(value);
        }
        index
    }

    /// The shortest value the index holds.
    #[must_use]
    pub fn min_chars(&self) -> usize {
        self.min_chars
    }

    /// How many distinct values (both normal forms counted separately) the index holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.exact.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// Adds one value in both normal forms. Short values are skipped.
    pub fn add(&mut self, value: &str) {
        for form in forms(value) {
            if form.chars().count() < self.min_chars {
                continue;
            }
            let digest = digest_of(&form);
            if self.exact.insert(digest) {
                if self.exact.len() > self.capacity {
                    self.grow();
                } else {
                    self.bloom.insert(&digest);
                }
            }
        }
    }

    /// Adds every original value inside a JSON value: strings, the text of numbers and booleans,
    /// and arrays and objects recursively. Call it only for a value that is being masked or
    /// stripped, so a kept amount never becomes a source value.
    pub fn add_json(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.add(text),
            Value::Number(number) => self.add(&number.to_string()),
            Value::Bool(flag) => self.add(&flag.to_string()),
            Value::Array(items) => {
                for item in items {
                    self.add_json(item);
                }
            }
            Value::Object(map) => {
                for item in map.values() {
                    self.add_json(item);
                }
            }
            Value::Null => {}
        }
    }

    /// Whether a candidate is an indexed value. The Bloom filter answers "no" cheaply; only the
    /// exact set may answer "yes".
    #[must_use]
    pub fn contains(&self, candidate: &str) -> Option<[u8; 32]> {
        for form in forms(candidate) {
            if form.chars().count() < self.min_chars {
                continue;
            }
            let digest = digest_of(&form);
            if self.bloom.might_contain(&digest) && self.exact.contains(&digest) {
                return Some(digest);
            }
        }
        None
    }

    fn grow(&mut self) {
        let target = (self.exact.len() * 2).max(16);
        self.capacity = target;
        self.bloom = Bloom::with_capacity(target);
        for digest in &self.exact {
            self.bloom.insert(digest);
        }
    }
}

/// Scans every surface of `store` for any original value in `index`.
#[must_use]
pub fn scan(store: &dyn SandboxStore, index: &SourceIndex) -> ScanReport {
    let mut report = ScanReport {
        tenant: store.tenant_id(),
        indexed_values: index.len(),
        ..ScanReport::default()
    };

    for document in store.documents() {
        for (field, value) in &document.fields {
            for text in strings_of(value) {
                bump(&mut report, Surface::Documents);
                if let Some(digest) = hit(index, &text) {
                    report.leaks.push(Leak {
                        surface: Surface::Documents,
                        doctype: document.doctype.clone(),
                        record: document.id.clone(),
                        field: field.clone(),
                        digest: hex(&digest[..8]),
                    });
                }
            }
        }
    }

    for row in store.journal() {
        for text in strings_of(&row.value) {
            bump(&mut report, Surface::Journal);
            if let Some(digest) = hit(index, &text) {
                report.leaks.push(Leak {
                    surface: Surface::Journal,
                    doctype: row.doctype.clone(),
                    record: row.doc_id.clone(),
                    field: row.field.clone(),
                    digest: hex(&digest[..8]),
                });
            }
        }
    }

    for row in store.audit() {
        for (side, value) in [("before", &row.before), ("after", &row.after)] {
            for text in strings_of(value) {
                bump(&mut report, Surface::Audit);
                if let Some(digest) = hit(index, &text) {
                    report.leaks.push(Leak {
                        surface: Surface::Audit,
                        doctype: row.doctype.clone(),
                        record: row.doc_id.clone(),
                        field: format!("{}[{side}]", row.field),
                        digest: hex(&digest[..8]),
                    });
                }
            }
        }
    }

    for row in store.search() {
        bump(&mut report, Surface::Search);
        if let Some(digest) = hit(index, &row.text) {
            report.leaks.push(Leak {
                surface: Surface::Search,
                doctype: row.doctype.clone(),
                record: row.doc_id.clone(),
                field: "text".to_owned(),
                digest: hex(&digest[..8]),
            });
        }
    }

    for attachment in store.attachments() {
        for (field, text) in [
            ("filename", attachment.filename.as_str()),
            ("content_hash", attachment.content_hash.as_str()),
            ("content", attachment.content.as_str()),
        ] {
            bump(&mut report, Surface::Attachments);
            if let Some(digest) = hit(index, text) {
                report.leaks.push(Leak {
                    surface: Surface::Attachments,
                    doctype: attachment.doctype.clone(),
                    record: attachment.id.clone(),
                    field: field.to_owned(),
                    digest: hex(&digest[..8]),
                });
            }
        }
    }

    for entry in store.caches() {
        for (field, text) in [("key", entry.key.as_str()), ("value", entry.value.as_str())] {
            bump(&mut report, Surface::Caches);
            if let Some(digest) = hit(index, text) {
                report.leaks.push(Leak {
                    surface: Surface::Caches,
                    doctype: "cache".to_owned(),
                    record: entry.key.clone(),
                    field: field.to_owned(),
                    digest: hex(&digest[..8]),
                });
            }
        }
    }

    report
}

/// Counts one read value for the surface totals.
fn bump(report: &mut ScanReport, surface: Surface) {
    *report
        .by_surface
        .entry(surface.as_str().to_owned())
        .or_insert(0) += 1;
    report.scanned_values += 1;
}

/// The digest of a match, or `None`. The full text and every run of up to eight words are
/// checked, so a value copied into a long comment is found even when it is one of several
/// words. Eight covers a name or an address without turning the scan quadratic.
fn hit(index: &SourceIndex, text: &str) -> Option<[u8; 32]> {
    if let Some(digest) = index.contains(text) {
        return Some(digest);
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 2 {
        return None;
    }
    for start in 0..words.len() {
        let longest = 8.min(words.len() - start);
        for length in 1..=longest {
            let window = words[start..start + length].join(" ");
            if let Some(digest) = index.contains(&window) {
                return Some(digest);
            }
        }
    }
    None
}

/// The two normal forms of a value: whitespace-collapsed lower case, and its alphanumeric
/// compact form.
fn forms(value: &str) -> Vec<String> {
    let normal = normalise(value);
    let compact = compact(value);
    if compact == normal {
        vec![normal]
    } else {
        vec![normal, compact]
    }
}

/// Lower-cased, trimmed, whitespace-collapsed.
#[must_use]
pub fn normalise(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = false;
    for ch in value.trim().chars() {
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        }
    }
    out.trim_end().to_owned()
}

/// Lower case, alphanumeric only: the form that survives punctuation and spacing differences.
#[must_use]
pub fn compact(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn digest_of(form: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(form.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Every string inside a JSON value, depth first.
fn strings_of(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_strings(value, &mut out);
    out
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// A fixed-size Bloom filter over 256-bit digests. Eight bits per item is more than the
/// ten-per-thousand false-positive rate needs at the sizes here; the exact set is the answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bloom {
    bits: Vec<u64>,
    hashes: u32,
}

impl Bloom {
    /// A filter sized for `items` with eight hash positions per item.
    #[must_use]
    pub fn with_capacity(items: usize) -> Self {
        let bit_count = (items.max(8) * 10).next_multiple_of(64).max(64);
        Self {
            bits: vec![0u64; bit_count / 64],
            hashes: 8,
        }
    }

    /// The number of bits in the filter.
    #[must_use]
    pub fn bit_len(&self) -> usize {
        self.bits.len() * 64
    }

    /// The number of hash positions per item.
    #[must_use]
    pub fn hashes(&self) -> u32 {
        self.hashes
    }

    /// Inserts a digest.
    pub fn insert(&mut self, digest: &[u8; 32]) {
        for position in self.positions(digest) {
            let word = position / 64;
            let bit = position % 64;
            if let Some(slot) = self.bits.get_mut(word) {
                *slot |= 1u64 << bit;
            }
        }
    }

    /// Whether every position is set. `false` is definitive; `true` is a maybe.
    #[must_use]
    pub fn might_contain(&self, digest: &[u8; 32]) -> bool {
        self.positions(digest).into_iter().all(|position| {
            self.bits
                .get(position / 64)
                .is_some_and(|word| word & (1u64 << (position % 64)) != 0)
        })
    }

    fn positions(&self, digest: &[u8; 32]) -> Vec<usize> {
        let mut first = [0u8; 8];
        let mut second = [0u8; 8];
        first.copy_from_slice(&digest[0..8]);
        second.copy_from_slice(&digest[8..16]);
        let h1 = u64::from_le_bytes(first);
        let h2 = u64::from_le_bytes(second) | 1;
        let len = self.bit_len() as u64;
        (0..self.hashes)
            .map(|index| {
                let value = h1.wrapping_add(u64::from(index).wrapping_mul(h2));
                (value % len) as usize
            })
            .collect()
    }
}

/// The leaks alone: the brief's `scan(sandbox, source_values) -> Vec<Leak>` shape, for a caller
/// that only wants the list. It is [`scan`] without the counts.
#[must_use]
pub fn scan_leaks(store: &dyn SandboxStore, index: &SourceIndex) -> Vec<Leak> {
    scan(store, index).leaks
}
