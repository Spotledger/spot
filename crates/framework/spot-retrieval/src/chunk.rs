//! Deterministic chunking: split one field value into overlapping windows that never exceed
//! their limit.
//!
//! A long value is cut at the coarsest boundary that fits: a **paragraph** break (`\n\n`),
//! then a **sentence** break (Unicode sentence boundaries), then a hard cut at the character
//! limit. Consecutive chunks overlap by up to `overlap_chars` characters, so a sentence that
//! straddles a boundary is still readable in one chunk; stripping each chunk's leading overlap
//! and concatenating the rest rebuilds the input exactly (the chunker loses nothing).
//!
//! Everything is counted in Unicode scalar values ("characters"), and a cut always lands on a
//! character boundary, so a chunk is always valid UTF-8. The result depends only on the input
//! and the two limits: the same value chunks the same way every run.

use unicode_segmentation::UnicodeSegmentation;

/// The character limit below which [`Chunking`](crate::Chunking) will not go.
pub const MIN_CHUNK_CHARS: usize = 1;

/// Splits `text` into chunks of at most `max_chunk_chars` characters, overlapping by up to
/// `overlap_chars`.
///
/// `overlap_chars` is clamped to `max_chunk_chars - 1`, so every chunk still makes progress and
/// the overlap can never swallow a whole chunk. An empty (or whitespace-only, after trimming
/// by the caller) value produces no chunks; the caller decides what "empty" means.
#[must_use]
pub fn chunk_text(text: &str, max_chunk_chars: usize, overlap_chars: usize) -> Vec<String> {
    let max = max_chunk_chars.max(MIN_CHUNK_CHARS);
    let overlap_cap = overlap_chars.min(max - 1);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }
    let paragraphs = paragraph_ends(&chars);
    let sentences = sentence_ends(text);

    let mut out: Vec<String> = Vec::new();
    let mut prev_end = 0usize;
    let mut prev_len = 0usize;
    loop {
        let overlap = if out.is_empty() {
            0
        } else {
            overlap_cap.min(prev_len.saturating_sub(1))
        };
        let start = prev_end - overlap;
        let budget = max - overlap;
        let window_end = (prev_end + budget).min(n);
        let end = choose_end(prev_end, window_end, n, &paragraphs, &sentences);
        let chunk: String = chars[start..end].iter().collect();
        out.push(chunk);
        prev_len = end - start;
        prev_end = end;
        if end >= n {
            break;
        }
    }
    out
}

/// The character index just past each paragraph break (`\n\n`), ascending.
fn paragraph_ends(chars: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 1usize;
    while i < chars.len() {
        if chars[i - 1] == '\n' && chars[i] == '\n' {
            out.push(i + 1);
        }
        i += 1;
    }
    out
}

/// The character index just past each Unicode sentence, ascending.
fn sentence_ends(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut total = 0usize;
    for sentence in text.unicode_sentences() {
        total += sentence.chars().count();
        out.push(total);
    }
    out
}

/// The furthest preferred boundary at or before `window_end`, never at or before
/// `body_start`; the hard limit when no boundary fits.
fn choose_end(
    body_start: usize,
    window_end: usize,
    n: usize,
    paragraphs: &[usize],
    sentences: &[usize],
) -> usize {
    if window_end >= n {
        return n;
    }
    let fit = |ends: &[usize]| {
        ends.iter()
            .rev()
            .copied()
            .find(|&e| e > body_start && e <= window_end)
    };
    fit(paragraphs)
        .or_else(|| fit(sentences))
        .unwrap_or(window_end)
}
