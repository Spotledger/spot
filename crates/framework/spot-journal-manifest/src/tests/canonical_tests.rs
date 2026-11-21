//! Tests of the canonical encoding: pinned bytes, the hash, the signature's message and the
//! decoder's refusals.

use crate::canonical::{decode_signed, manifest_bytes, signed_bytes};
use crate::{
    MANIFEST_TAG, Manifest, ManifestError, SegmentSummary, SignedManifest, manifest_hash, sign,
};

use super::{hash, key, key_id};

/// The lower-case hex of `bytes`.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The bytes a hex string spells.
fn from_hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "hex spells whole bytes");
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex"))
        .collect()
}

/// The manifest every golden vector is about: two segments, no fork, no previous link, a
/// millisecond timestamp, and hashes whose bytes are easy to read.
fn golden() -> Manifest {
    Manifest {
        source: "site:demo".to_owned(),
        generation: 1,
        segments: vec![
            SegmentSummary::new(1, 1, 3, 3, hash(0x11), hash(0x00)),
            SegmentSummary::new(1, 4, 10, 7, hash(0x22), hash(0x11)),
        ],
        forks: Vec::new(),
        previous_manifest_hash: None,
        issued_at_millis: 1_700_000_000_000,
    }
}

/// The canonical bytes of [`golden`], field by field, pinned.
const GOLDEN_BYTES: &str = concat!(
    // the tag.
    "73706f742e6a6f75726e616c2e6d616e69666573742e763100",
    // field(source): u32(9) then "site:demo".
    "00000009736974653a64656d6f",
    // u64(generation) = 1.
    "0000000000000001",
    // u32(segment_count) = 2.
    "00000002",
    // segment 1: generation, first_seq, last_seq, entry_count.
    "0000000000000001000000000000000100000000000000030000000000000003",
    // segment 1: segment_hash = 0x11 repeated.
    "1111111111111111111111111111111111111111111111111111111111111111",
    // segment 1: previous_hash = 0x00 repeated.
    "0000000000000000000000000000000000000000000000000000000000000000",
    // segment 2: generation, first_seq, last_seq, entry_count.
    "00000000000000010000000000000004000000000000000a0000000000000007",
    // segment 2: segment_hash = 0x22 repeated.
    "2222222222222222222222222222222222222222222222222222222222222222",
    // segment 2: previous_hash = 0x11 repeated.
    "1111111111111111111111111111111111111111111111111111111111111111",
    // u32(fork_count) = 0.
    "00000000",
    // opt32(previous_manifest_hash) = 0, no link.
    "00",
    // i64(issued_at_millis) = 1_700_000_000_000.
    "0000018bcfe56800",
);

/// `SHA-256` of [`GOLDEN_BYTES`], pinned.
const GOLDEN_HASH: &str = "ed5e554e0bbd616be0c4f793dc4b0a878fa300e85926373766d487979b7fd5ee";

/// The Ed25519 signature of [`GOLDEN_BYTES`] followed by `field(key_id)`, under seed 7, pinned
/// (Ed25519 signing is deterministic).
const GOLDEN_SIGNATURE: &str = "800a5f2f48d815c1b74b5e5f5111f8d1065f662b4e92fc194498b04b2b8894755f97eac73961a9b14989b8ee4164690269f55c9824f53af4d8ad817a563c7802";

/// The whole signed manifest's bytes: the manifest bytes, then `field(key_id)` and
/// `field(signature)`.
fn golden_signed() -> String {
    format!("{GOLDEN_BYTES}0000000d6a6f75726e616c2d6b65792d3700000040{GOLDEN_SIGNATURE}")
}

#[test]
fn the_canonical_bytes_are_pinned() {
    assert_eq!(to_hex(&manifest_bytes(&golden())), GOLDEN_BYTES);
    assert_eq!(from_hex(GOLDEN_BYTES), manifest_bytes(&golden()));
    assert_eq!(manifest_bytes(&golden()).len(), 255);
}

#[test]
fn the_manifest_hash_is_pinned() {
    assert_eq!(to_hex(&manifest_hash(&golden())), GOLDEN_HASH);
    assert_eq!(golden().manifest_hash(), manifest_hash(&golden()));
}

#[test]
fn the_signature_and_the_signed_bytes_are_pinned() {
    let signed = sign(&golden(), &key(7), &key_id(7));
    assert_eq!(to_hex(&signed.signature), GOLDEN_SIGNATURE);
    assert_eq!(to_hex(&signed.to_bytes()), golden_signed());
    let decoded =
        SignedManifest::from_bytes(&from_hex(&golden_signed())).expect("the vector decodes");
    assert_eq!(decoded, signed);
}

#[test]
fn the_tag_opens_the_bytes() {
    let bytes = manifest_bytes(&golden());
    assert_eq!(&bytes[..MANIFEST_TAG.len()], MANIFEST_TAG);
    assert_eq!(&bytes[..21], b"spot.journal.manifest");
}

#[test]
fn every_variable_length_field_is_length_prefixed() {
    let bytes = manifest_bytes(&golden());
    let tag = MANIFEST_TAG.len();
    // source: u32 length then its bytes.
    assert_eq!(&bytes[tag..tag + 4], &[0, 0, 0, 9]);
    assert_eq!(&bytes[tag + 4..tag + 13], b"site:demo");
    // generation, right after the source.
    assert_eq!(&bytes[tag + 13..tag + 21], &[0, 0, 0, 0, 0, 0, 0, 1]);
    // the segment count.
    assert_eq!(&bytes[tag + 21..tag + 25], &[0, 0, 0, 2]);
}

#[test]
fn the_encoding_is_stable_across_calls() {
    let manifest = golden();
    assert_eq!(manifest_bytes(&manifest), manifest_bytes(&manifest));
    assert_eq!(manifest_hash(&manifest), manifest_hash(&manifest));
}

#[test]
fn a_field_change_changes_the_bytes() {
    let base = golden();
    let mut one = golden();
    one.issued_at_millis += 1;
    let mut two = golden();
    two.segments[1].entry_count += 1;
    let mut three = golden();
    three.source.push('x');
    for other in [one, two, three] {
        assert_ne!(manifest_bytes(&base), manifest_bytes(&other));
        assert_ne!(manifest_hash(&base), manifest_hash(&other));
    }
}

#[test]
fn signed_bytes_append_the_key_id() {
    let manifest = golden();
    let id = key_id(7);
    let mut expected = manifest_bytes(&manifest);
    expected.extend_from_slice(
        &u32::try_from(id.len())
            .expect("a short key id")
            .to_be_bytes(),
    );
    expected.extend_from_slice(id.as_bytes());
    assert_eq!(signed_bytes(&manifest, &id), expected);
    assert_eq!(sign(&manifest, &key(7), &id).canonical_bytes(), expected);
}

#[test]
fn the_byte_form_round_trips() {
    let signed = sign(&golden(), &key(7), &key_id(7));
    let decoded = SignedManifest::from_bytes(&signed.to_bytes()).expect("canonical bytes decode");
    assert_eq!(decoded, signed);
}

#[test]
fn the_byte_form_refuses_a_wrong_tag() {
    let signed = sign(&golden(), &key(7), &key_id(7));
    let mut bytes = signed.to_bytes();
    bytes[0] = b'X';
    assert!(matches!(
        SignedManifest::from_bytes(&bytes),
        Err(ManifestError::FormatInvalid(_))
    ));
}

#[test]
fn the_byte_form_refuses_truncation() {
    let signed = sign(&golden(), &key(7), &key_id(7));
    let bytes = signed.to_bytes();
    for cut in [0, 1, MANIFEST_TAG.len(), bytes.len() - 1] {
        assert!(matches!(
            SignedManifest::from_bytes(&bytes[..cut]),
            Err(ManifestError::FormatInvalid(_))
        ));
    }
}

#[test]
fn the_byte_form_refuses_trailing_data() {
    let signed = sign(&golden(), &key(7), &key_id(7));
    let mut bytes = signed.to_bytes();
    bytes.push(0);
    assert!(matches!(
        SignedManifest::from_bytes(&bytes),
        Err(ManifestError::FormatInvalid(_))
    ));
}

#[test]
fn the_byte_form_refuses_an_unknown_optional_hash_flag() {
    let mut bytes = manifest_bytes(&golden());
    // The previous-manifest flag is the byte before the issued-at timestamp (the last 8 bytes).
    let flag = bytes.len() - 9;
    assert_eq!(bytes[flag], 0, "the golden manifest has no previous link");
    bytes[flag] = 2;
    assert!(matches!(
        decode_signed(&bytes),
        Err(ManifestError::FormatInvalid(_))
    ));
}

#[test]
fn a_negative_timestamp_and_a_multibyte_reason_round_trip() {
    let mut manifest = golden();
    manifest.generation = 2;
    manifest.segments = vec![SegmentSummary::new(2, 1, 2, 2, hash(0x44), hash(0x55))];
    manifest.forks = vec![crate::Fork::new(2, 5, "point-in-time rebuild: café ☕", -1)];
    manifest.previous_manifest_hash = Some(hash(0x66));
    manifest.issued_at_millis = i64::MIN;
    manifest.validate().expect("the manifest is valid");
    let signed = sign(&manifest, &key(7), &key_id(7));
    let decoded = SignedManifest::from_bytes(&signed.to_bytes()).expect("the bytes decode");
    assert_eq!(decoded.manifest, manifest);
    assert_eq!(
        decoded.manifest.forks[0].reason,
        "point-in-time rebuild: café ☕"
    );
    assert_eq!(decoded.manifest.forks[0].at_millis, -1);
    assert_eq!(decoded.manifest.issued_at_millis, i64::MIN);
}
