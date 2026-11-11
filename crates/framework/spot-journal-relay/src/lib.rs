//! The journal relay (WP-JRN-03.T4, ADR-061 s5 and s10).
//!
//! A library over traits that takes batches of event-envelope lines for **one tenant and one
//! source** and makes them durable in the journal store in the right order, keeping the signed
//! manifest true. It composes `spot-journal-segment` (the sealed format and the sealing policy),
//! `spot-journal-manifest` (the signed, append-only manifest) and `spot-journal-store` (the key
//! layout and the object-store seam); it adds no format of its own except the tail chunk.
//!
//! # The flow
//!
//! 1. [`Relay::open`] (generation 1) or [`Relay::open_fork`] (a later generation, from a fork
//!    record) creates the generation's signed manifest if there is none and then runs
//!    [`Relay::resume`].
//! 2. [`Relay::append_batch`] checks the batch (strictly consecutive positions of the relay's
//!    generation; nothing is written for a bad batch), seals the open segment first when it is
//!    due, and then:
//!    * under [`Durability::RemoteSync`] writes the batch as an immutable tail chunk and only
//!      then returns [`Appended::ChunkDurable`], the value the acknowledgement gate consumes;
//!    * under `local` and `replica-sync` holds the entries for the open segment and returns
//!      [`Appended::Buffered`]; no chunk is written.
//! 3. **Sealing** runs three steps in this order, each idempotent: (a) `put_new` the segment;
//!    (b) append its summary to the manifest and `put_replace` the signed manifest with the etag
//!    read at the start of the step; (c) delete the tail chunks the segment now covers.
//! 4. [`Relay::resume`] repairs a crash between any two steps: it adopts a segment object the
//!    manifest lacks (header range, hash chain and blocks verified first), deletes chunks the
//!    manifest already covers and re-seals the chunks it still has. It never overwrites: when
//!    the stored chain disagrees with the manifest it stops with `journal_relay.chain_broken`.
//!
//! # One writer
//!
//! The manifest is the lock. A guarded manifest replace that loses (or a manifest that changed
//! since the relay last saw it, or a tail chunk or segment written by someone else) means
//! another relay owns the source: the relay stops with `journal_relay.split_brain` and writes
//! nothing further, on this or any later call.
//!
//! # What a failure leaves
//!
//! A call that fails after it started writing leaves the relay unsure what the store holds, so
//! every call but [`Relay::resume`] answers `journal_relay.resume_required` until a resume
//! succeeds. After a resume, [`Relay::next_position`] says where the caller must continue: under
//! `remote-sync` a batch whose chunk was written is already held (replaying it is an
//! `entry_out_of_order`), and under `local` and `replica-sync` entries that were never sealed
//! are lost to the relay if the process died, so the caller replays from `next_position`.
//!
//! # Tenancy and secrets
//!
//! The tenant id is fixed at construction and used for every key; no call takes a key or a
//! tenant. Entry lines are opaque and never appear in an error or a log (the crate logs
//! nothing). The signing key lives behind [`ManifestSigner`] and never enters the crate.
//!
//! # Not here
//!
//! Encryption of segments, the acknowledgement gate, multi-tenant scheduling and the outbox
//! reader are later tasks. Time (`now_millis`) is an argument everywhere; the crate reads no
//! clock and no random source.

#![forbid(unsafe_code)]

mod chunk;
mod config;
mod error;
mod relay;
mod resume;
mod signer;

#[cfg(test)]
mod tests;

pub use config::{Appended, Batch, Durability, RelayConfig};
pub use error::RelayError;
pub use relay::Relay;
pub use signer::ManifestSigner;
