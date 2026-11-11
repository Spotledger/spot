//! Property tests: any edit to a signed manifest's bytes fails to verify, any manifest round-trips
//! through its bytes, and a manifest always agrees with the segments it lists.

use proptest::prelude::*;

use crate::{SignedManifest, manifest_hash, sign, verify};

use super::{key, key_id, keys, manifest, tiling};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Editing the canonical bytes of a signed manifest never yields a manifest that verifies.
    ///
    /// The edits are bit flips at random offsets, so they land in the tag, a length prefix, an
    /// integer, a hash, the key id or the signature. A mutant that still decodes must therefore
    /// carry different canonical bytes than the manifest that was signed, and its signature must
    /// fail.
    #[test]
    fn random_edits_to_the_canonical_bytes_never_verify(
        segments in 1_usize..4,
        span in 1_u64..6,
        edits in prop::collection::vec((any::<prop::sample::Index>(), 1_u8..=255), 1..5),
    ) {
        let total = u64::try_from(segments).expect("a small count") * span;
        let base = manifest("site:demo", 1, tiling(1, total, span, 3));
        let signed = sign(&base, &key(7), &key_id(7));
        let keys = keys(7, 1, 10);
        prop_assert!(verify(&signed, &keys).is_ok());

        let bytes = signed.to_bytes();
        let mut edited = bytes.clone();
        for (index, byte) in &edits {
            let at = index.index(edited.len());
            edited[at] ^= byte;
        }
        prop_assume!(edited != bytes);
        if let Ok(mutant) = SignedManifest::from_bytes(&edited) {
            prop_assert!(verify(&mutant, &keys).is_err());
        }
    }

    /// Every valid manifest survives its byte form unchanged, and the hash of the decoded manifest
    /// is the hash of the original.
    #[test]
    fn any_valid_manifest_round_trips_through_its_bytes(
        source in "[a-z0-9:.# -]{1,24}",
        generation in 1_u64..4,
        segments in 0_usize..5,
        span in 1_u64..4,
        issued_at_millis in any::<i64>(),
    ) {
        prop_assume!(!source.trim().is_empty());
        let total = u64::try_from(segments).expect("a small count") * span;
        let mut base = manifest(&source, generation, tiling(generation, total, span, 5));
        base.issued_at_millis = issued_at_millis;
        prop_assert!(base.validate().is_ok());

        let signed = sign(&base, &key(7), &key_id(7));
        let decoded = SignedManifest::from_bytes(&signed.to_bytes()).expect("canonical bytes decode");
        prop_assert_eq!(&decoded, &signed);
        prop_assert_eq!(manifest_hash(&decoded.manifest), manifest_hash(&base));
        prop_assert!(verify(&decoded, &keys(7, 1, 10)).is_ok());
    }

    /// A verified manifest always agrees with the segments it lists, whatever it lists.
    #[test]
    fn a_manifest_never_contradicts_the_segments_it_lists(
        segments in 0_usize..6,
        span in 1_u64..5,
        extra in 0_u64..3,
    ) {
        let total = u64::try_from(segments).expect("a small count") * span + extra;
        let base = manifest("site:demo", 1, tiling(1, total, span, 7));
        prop_assert!(base.validate().is_ok());
        let verified = verify(&sign(&base, &key(7), &key_id(7)), &keys(7, 1, 10))
            .expect("the test key verifies");
        prop_assert!(verified.check_against(&base.segments).is_empty());
    }

    /// Appending the segments a manifest does not list yet keeps every earlier summary and never
    /// changes the generation.
    #[test]
    fn appending_is_monotone(
        span in 1_u64..5,
        count in 1_usize..6,
        keep in 0_usize..5,
    ) {
        let total = u64::try_from(count).expect("a small count") * span;
        let chain = tiling(1, total, span, 9);
        let split = keep.min(chain.len());
        let (head, tail) = chain.split_at(split);
        let start = manifest("site:demo", 1, head.to_vec());
        prop_assert!(start.validate().is_ok());

        let grown = crate::append_segments(&start, tail).expect("contiguous segments append");
        prop_assert_eq!(grown.generation, start.generation);
        prop_assert_eq!(&grown.segments[..head.len()], head);
        prop_assert_eq!(&grown.segments, &chain);
        prop_assert_eq!(grown.last_seq(), total);
    }
}
