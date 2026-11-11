//! The journal's object-store seam (WP-JRN-03.T3, ADR-061 section 5).
//!
//! The relay writes and a recovery reads; both go through [`JournalStore`], and both speak keys
//! built by [`Layout`]. Nothing else in this crate knows a key shape.
//!
//! # What is here
//!
//! * [`Layout`] — the five per-tenant key shapes of ADR-061 s5, as typed constructors and one
//!   strict parser ([`layout`] carries the form).
//! * [`JournalStore`] — six methods over a flat key space, all returning boxed futures
//!   ([`StoreFuture`]), so this crate pulls in no async runtime and a caller on any executor (or
//!   a test with a hand-rolled block-on) can drive them.
//! * [`MemoryStore`] — the reference backend and what a relay unit test uses.
//! * [`FolderStore`] — one directory, one file per object, atomic writes, create-new semantics:
//!   the standalone profile's backend.
//! * [`contract::run`] — the suite every backend runs. The in-memory and folder stores pass it
//!   here; an S3 or Azure provider added later must pass the same suite unchanged.
//! * [`StoreError`] — typed refusals with the stable `journal_store.*` codes catalogued in
//!   `crates/framework/spot-errors/catalogue/journal_store.toml`.
//!
//! # What this crate deliberately does not do
//!
//! No network, no clock, no randomness, no async runtime, no database: the store is given a key
//! and bytes and hands them back. Compression, checksums, signing and the manifest's content
//! belong to the journal's other crates (`spot-journal-segment` writes segments), and the relay
//! that decides *what* to write is a later task. Nothing here logs, and no message carries object
//! bytes.
//!
//! # The rules a backend must hold
//!
//! Listed on [`JournalStore`] and checked by [`contract::run`]: canonical keys only; immutability
//! under `put_new` (one winner per key, even under threads); an etag-guarded replace of the
//! manifest alone; ordered, resumable listing; deletion of tail chunks only, never of a segment
//! or a snapshot.
//!
//! # Etags
//!
//! The etag of an object is the lower-case hex SHA-256 of exactly its bytes ([`etag`]), for every
//! backend. It is the guard [`JournalStore::put_replace`] compares, and it is what makes a
//! manifest's "nothing changed since this etag" check meaningful across providers.
//!
//! # Example
//!
//! ```
//! use spot_journal_store::{JournalStore, Layout, MemoryStore};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = MemoryStore::new();
//! let key = Layout::tail_chunk("acme", 7, 1)?.to_key();
//! assert_eq!(key, "acme/tail/00000000000000000007/00000000000000000001.chunk");
//!
//! let future = store.put_new(&key, b"entries".to_vec());
//! // A caller on any runtime awaits it; here a plain block-on does.
//! let meta = block_on(future)?;
//! assert_eq!(meta.size, 7);
//! # Ok(())
//! # }
//! # fn block_on<F: std::future::Future>(future: F) -> F::Output {
//! #     let mut future = std::pin::pin!(future);
//! #     let waker = std::task::Waker::noop();
//! #     let mut context = std::task::Context::from_waker(waker);
//! #     loop {
//! #         match std::future::Future::poll(future.as_mut(), &mut context) {
//! #             std::task::Poll::Ready(value) => return value,
//! #             std::task::Poll::Pending => std::thread::yield_now(),
//! #         }
//! #     }
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod contract;
mod error;
mod folder;
mod layout;
mod memory;
mod store;

#[cfg(test)]
mod tests;

pub use error::StoreError;
pub use folder::{FolderStore, STAGING_DIR};
pub use layout::{
    BLOBS_PREFIX, BlobKey, DIGITS, Key, Layout, MANIFEST_PREFIX, MAX_TENANT_BYTES, ManifestKey,
    ObjectKind, SEGMENTS_PREFIX, SNAPSHOTS_PREFIX, SegmentKey, SnapshotKey, TAIL_PREFIX, TailKey,
    TenantId,
};
pub use memory::MemoryStore;
pub use store::{JournalStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, etag};
