//! The tail chunk codec.

use crate::chunk::{decode, encode};

use super::support::{input, line, pos};

#[test]
fn a_chunk_round_trips() {
    let entries = input(1, 5, 9);
    let bytes = encode(&entries).expect("encodes");
    assert_eq!(decode(&bytes), Some(entries));
}

#[test]
fn a_chunk_with_one_changed_byte_is_refused() {
    let bytes = encode(&input(1, 1, 3)).expect("encodes");
    for index in [
        0,
        9,
        12,
        20,
        bytes.len() / 2,
        bytes.len() - 33,
        bytes.len() - 1,
    ] {
        let mut damaged = bytes.clone();
        damaged[index] ^= 0x01;
        assert_eq!(decode(&damaged), None, "byte {index}");
    }
}

#[test]
fn a_truncated_or_extended_chunk_is_refused() {
    let bytes = encode(&input(1, 1, 3)).expect("encodes");
    for cut in [0, 1, 8, 31, 32, bytes.len() - 1] {
        assert_eq!(decode(&bytes[..cut]), None, "cut at {cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(decode(&longer), None);
}

#[test]
fn an_empty_batch_encodes_and_decodes_to_nothing() {
    let bytes = encode(&[]).expect("encodes");
    assert_eq!(decode(&bytes), Some(Vec::new()));
    let one = encode(&[(pos(1), line(1, 1))]).expect("encodes");
    assert_eq!(decode(&one).map(|e| e.len()), Some(1));
}
