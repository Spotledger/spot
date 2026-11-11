//! The signed journal manifest (WP-JRN-03.T2, ADR-061 s5 and s8).
//!
//! The manifest is the ordered list of one generation's sealed journal segments: each entry names
//! the positions a segment holds, how many entries it carries and the hash of its bytes, plus the
//! hash of the segment before it, so the list is a hash chain. The manifest also carries the
//! source's fork history (a point-in-time rebuild abandons a generation and opens a new one) and
//! the hash of the manifest before it. It is signed with the tenant's journal key, and a verifier
//! with the tenant's public keys proves it has not changed.
//!
//! This is a pure library: keys, timestamps and ids are arguments, and it reads no clock, no
//! randomness and no storage. It works on the summaries it defines ([`SegmentSummary`]) and never
//! on segment bytes; the segment format itself is `spot-journal-segment` (WP-JRN-03.T1) and the
//! tie between the two is a later integration task.
//!
//! # Using it
//!
//! 1. [`Manifest::first`] opens a source's generation 1.
//! 2. [`append_segments`] grows a manifest: contiguous positions only, earlier summaries untouched.
//! 3. [`Manifest::fork`] opens the next generation and records why, keeping the old one verifiable.
//! 4. [`sign`] signs a manifest under a key id; [`verify`] checks it against a
//!    [`VerifyingKeys`] set and returns a [`Verified`] value that only a valid manifest can be.
//! 5. [`Verified::check_against`] compares the verified manifest with the summaries storage holds
//!    and reports [`Finding`]s.
//!
//! # Layout (manifest encoding version 1, every integer big-endian)
//!
//! ```text
//! manifest = tag field(source) u64(generation) u32(segment_count) segment*
//!            u32(fork_count) fork* opt32(previous_manifest_hash) i64(issued_at_millis)
//! segment  = u64(generation) u64(first_seq) u64(last_seq) u64(entry_count)
//!            [32](segment_hash) [32](previous_hash)
//! fork     = u64(generation) u64(forked_from_position) field(reason) i64(at_millis)
//! field    = u32(length) bytes
//! opt32    = 0   |   1 [32]
//! tag      = "spot.journal.manifest.v1\0"
//! signed   = manifest field(key_id) field(signature)
//! ```
//!
//! * **No floating point anywhere.** Timestamps are `i64` Unix milliseconds supplied by the
//!   caller; there is no duration, ratio or amount in the format.
//! * **Every variable-length field is length-prefixed** (`field`), and every hash is a fixed 32
//!   bytes with no prefix, so the encoding is unambiguous: two different manifests can never
//!   produce the same bytes and one manifest has exactly one byte form.
//! * **The tag** makes a manifest's bytes distinguishable from any other signed document, so a
//!   signature over a manifest cannot be replayed over another type.
//! * **[`manifest_hash`]** is `SHA-256` of the canonical bytes; the next manifest of a source
//!   carries it as `previous_manifest_hash`, which links the generations.
//! * **A signature covers the canonical bytes and then the key id** (`signed`), so the key id
//!   cannot be changed without breaking the signature and a signature cannot be moved to a
//!   manifest that names another key.
//! * **[`SignedManifest::to_bytes`]** appends the length-prefixed signature and
//!   [`SignedManifest::from_bytes`] refuses any byte string that does not re-encode to itself, so
//!   the decoder is the encoder's faithful inverse.
//! * **Counts are `u32`:** more than `u32::MAX` segments or forks in one manifest is not a size
//!   the format can express (each segment alone occupies 96 bytes).

mod canonical;
mod error;
mod findings;
mod manifest;
mod sign;
mod verify;

#[cfg(test)]
mod tests;

pub use canonical::{MANIFEST_TAG, manifest_bytes, manifest_hash};
pub use error::ManifestError;
pub use findings::{Finding, check_against};
pub use manifest::{Fork, Manifest, SegmentSummary, append_segments};
pub use sign::{SignedManifest, VerifyingKeys, sign};
pub use verify::{Verified, verify};
