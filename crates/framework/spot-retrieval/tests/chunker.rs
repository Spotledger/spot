//! Chunker property tests: no chunk over its limit, overlap-stripped concatenation rebuilds the
//! text, and chunking is deterministic.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_retrieval::chunk_text;

/// The text a chunk list carries, with each chunk's leading overlap stripped.
fn rebuild(chunks: &[String], overlap_chars: usize) -> String {
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            out.push_str(chunk);
        } else {
            let prev_len = chunks[i - 1].chars().count();
            let overlap = overlap_chars.min(prev_len.saturating_sub(1));
            out.extend(chunk.chars().skip(overlap));
        }
    }
    out
}

proptest! {
    #[test]
    fn no_chunk_exceeds_its_limit(text in ".*", max in 1usize..200, overlap in 0usize..200) {
        for chunk in chunk_text(&text, max, overlap) {
            prop_assert!(chunk.chars().count() <= max, "a chunk exceeded {max} characters");
        }
    }

    #[test]
    fn overlap_stripped_concatenation_rebuilds_the_text(
        text in ".*",
        max in 1usize..200,
        overlap in 0usize..200,
    ) {
        let chunks = chunk_text(&text, max, overlap);
        prop_assert_eq!(rebuild(&chunks, overlap), text);
    }

    #[test]
    fn chunking_is_deterministic(text in ".*", max in 1usize..200, overlap in 0usize..200) {
        prop_assert_eq!(chunk_text(&text, max, overlap), chunk_text(&text, max, overlap));
    }
}

#[test]
fn an_empty_value_has_no_chunks() {
    assert!(chunk_text("", 10, 2).is_empty());
}

#[test]
fn a_short_value_is_one_chunk() {
    assert_eq!(chunk_text("hello", 10, 2), vec!["hello".to_owned()]);
}

#[test]
fn paragraphs_and_sentences_are_preferred_boundaries() {
    let text = "First paragraph.\n\nSecond paragraph has two sentences. The second one here.";
    let chunks = chunk_text(text, 30, 0);
    assert!(chunks.len() > 1);
    // The first break is the paragraph break, so the first chunk ends there.
    assert_eq!(chunks[0], "First paragraph.\n\n");
}
