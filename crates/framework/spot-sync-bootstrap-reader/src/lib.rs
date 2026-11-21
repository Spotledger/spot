//! `spot-sync-bootstrap-reader` (WP-SYN-07.T7): the [`SnapshotSource`] over a real snapshot.
//!
//! `spot-sync-bootstrap` plans and serves a snapshot bootstrap through the synchronous trait
//! [`SnapshotSource`] and is free of an async runtime and of the object store. The snapshot itself
//! is read by the async `spot_journal_snapshot::SnapshotReader`. [`ReaderSource`] is the adapter
//! between the two: the host gives it a reader, the snapshot's [`SnapshotInfo`] (from
//! `list_snapshots`), the tenant it expects and a [`Blocker`], and passes it to
//! `Bootstrap::start`.
//!
//! # The two reads
//!
//! - [`SnapshotSource::scan`] drives the reader's image stream once, calling the visitor for each
//!   image in ascending `(entity, id)` order and holding **one image at a time** (the stream
//!   decompresses one part at a time). While it streams it records the **first key of each part**,
//!   one key per part, so a later `scan` or `fetch` reuses the vector.
//! - [`SnapshotSource::fetch`] answers exactly the requested keys. Each key is assigned to its part
//!   by binary search over the first keys (a key below the first part's first key is absent), each
//!   needed part is loaded **once** through `SnapshotReader::part` (verified exactly as streaming
//!   verifies), the images whose key was asked for are kept and the part is dropped before the next
//!   one: at most one decompressed part is alive at a time, and the parts a chunk's keys do not live
//!   in are never read.
//!
//! # Cost model
//!
//! One `scan` to plan (every part once) builds the first-key vector. After that a chunk of `k` keys
//! reads at most `k` parts, normally a handful, because the bootstrap serves keys in group order and
//! the snapshot is in key order. A `fetch` before any `scan` first builds the vector with one forward
//! pass over every part keeping only each part's first key (the same cost as a scan; the bootstrap
//! always scans first, so this is a safety net, not the path).
//!
//! # The [`Blocker`] rule
//!
//! The adapter is synchronous and the reader is async, so the host supplies a [`Blocker`] that runs
//! one future to completion. It **must not be called from a thread that is itself running the async
//! runtime** (it would block the runtime that must drive the store): call the bootstrap from a
//! blocking task or a thread of its own. This crate gives no runtime-specific blocker outside its
//! tests.
//!
//! # Errors
//!
//! Every `SnapshotError` becomes `BootstrapError::SourceFailed { reason }`, with a reason built from
//! the error's stable code (and the part's object name, which is allowed): never an image, an entity
//! or an id.
//!
//! # Wiring
//!
//! ```ignore
//! let reader = SnapshotReader::open(&store, tenant, generation, position, &keys).await?;
//! let info = list_snapshots(&store, tenant, &keys).await?.into_iter().find(..).unwrap();
//! let source = ReaderSource::open(reader, info, tenant, MyBlocker)?;
//! let session = Bootstrap::start(&source, seams, limits, &node, hub, StartOptions::default())?;
//! ```
//!
//! The crate reads no clock and no randomness, and holds no image beyond the one being visited
//! (`scan`) or the one part being searched (`fetch`).

#![forbid(unsafe_code)]

mod error;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use spot_journal_snapshot::{ImageRecord, SnapshotError, SnapshotInfo, SnapshotReader};
use spot_sync_bootstrap::{BootstrapError, SnapshotSource};
use spot_sync_closure::RecordKey;

pub use error::OpenError;

/// Runs one future to completion from synchronous code.
///
/// The rule: never call it from a thread that is itself running the async runtime that drives the
/// store; call the bootstrap from a blocking task. The future is not required to be `Send`: it
/// borrows the reader and the caller's visitor, and runs on the calling thread.
pub trait Blocker {
    /// Runs `future` to completion and returns its output.
    fn block_on<T>(&self, future: Pin<Box<dyn Future<Output = T> + '_>>) -> T;
}

/// The first key of each non-empty part, with the part's ordinal, in key order.
type FirstKeys = Vec<(RecordKey, usize)>;

/// A [`SnapshotSource`] over an async `SnapshotReader`.
pub struct ReaderSource<'a, B: Blocker> {
    reader: SnapshotReader<'a>,
    info: SnapshotInfo,
    tenant: String,
    blocker: B,
    first_keys: Mutex<Option<FirstKeys>>,
}

impl<'a, B: Blocker> ReaderSource<'a, B> {
    /// Opens `reader` as a source.
    ///
    /// Checks that `info.position` and `info.generation` equal the verified index body's and that
    /// the index belongs to `expected_tenant`; nothing is read from a part here.
    ///
    /// # Errors
    /// [`OpenError::TenantMismatch`] (checked first, so a snapshot of another tenant is refused
    /// before anything else is compared) and [`OpenError::SnapshotMismatch`].
    pub fn open(
        reader: SnapshotReader<'a>,
        info: SnapshotInfo,
        expected_tenant: &str,
        blocker: B,
    ) -> Result<Self, OpenError> {
        let body = &reader.index().body;
        if body.tenant != expected_tenant {
            return Err(OpenError::TenantMismatch);
        }
        if body.position != info.position || body.generation != info.generation {
            return Err(OpenError::SnapshotMismatch {
                info_generation: info.generation,
                info_position: info.position,
                index_generation: body.generation,
                index_position: body.position,
            });
        }
        let tenant = body.tenant.clone();
        Ok(Self {
            reader,
            info,
            tenant,
            blocker,
            first_keys: Mutex::new(None),
        })
    }

    /// How many parts the snapshot has.
    #[must_use]
    pub fn part_count(&self) -> usize {
        self.reader.part_count()
    }

    fn remembered(&self) -> Option<FirstKeys> {
        self.first_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn remember(&self, keys: FirstKeys) {
        *self
            .first_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(keys);
    }

    /// The first key of every non-empty part, built with one forward pass over the parts when no
    /// scan has run yet. Each part is dropped before the next is loaded.
    async fn first_keys(&self) -> Result<FirstKeys, BootstrapError> {
        if let Some(keys) = self.remembered() {
            return Ok(keys);
        }
        let mut firsts: FirstKeys = Vec::new();
        for ordinal in 0..self.reader.part_count() {
            let part = self.reader.part(ordinal).await.map_err(source_failed)?;
            if let Some(first) = part.first() {
                firsts.push((RecordKey::new(&first.entity, &first.id), ordinal));
            }
        }
        self.remember(firsts.clone());
        Ok(firsts)
    }

    async fn scan_async(
        &self,
        visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
    ) -> Result<(), BootstrapError> {
        let counts: Vec<u64> = self
            .reader
            .index()
            .body
            .parts
            .iter()
            .map(|part| part.images)
            .collect();
        let build = self.remembered().is_none();
        let mut firsts: FirstKeys = Vec::new();
        let mut stream = self.reader.images();
        let (mut part, mut seen_in_part) = (0_usize, 0_u64);
        while let Some(record) = stream.next().await.map_err(source_failed)? {
            // Move past parts that hold no image (the writer never writes one; be exact anyway).
            while counts.get(part).is_some_and(|count| seen_in_part >= *count) {
                part += 1;
                seen_in_part = 0;
            }
            if build && seen_in_part == 0 {
                firsts.push((RecordKey::new(&record.entity, &record.id), part));
            }
            seen_in_part += 1;
            visit(record)?;
        }
        if build {
            self.remember(firsts);
        }
        Ok(())
    }

    async fn fetch_async(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let firsts = self.first_keys().await?;
        // Which part holds each key, if any part can: the last part whose first key is not after it.
        let mut wanted: BTreeMap<usize, BTreeSet<&RecordKey>> = BTreeMap::new();
        for key in keys {
            let after = firsts.partition_point(|(first, _)| first <= key);
            if let Some(index) = after.checked_sub(1)
                && let Some((_, ordinal)) = firsts.get(index)
            {
                wanted.entry(*ordinal).or_default().insert(key);
            }
        }
        let mut found: Vec<ImageRecord> = Vec::new();
        for (ordinal, asked) in wanted {
            // One part at a time, dropped before the next.
            let part = self.reader.part(ordinal).await.map_err(source_failed)?;
            found.extend(part.into_iter().filter(|record| {
                asked
                    .iter()
                    .any(|key| key.entity == record.entity && key.id == record.id)
            }));
        }
        Ok(found)
    }
}

impl<B: Blocker> SnapshotSource for ReaderSource<'_, B> {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn info(&self) -> SnapshotInfo {
        self.info
    }

    fn scan(
        &self,
        visit: &mut dyn FnMut(ImageRecord) -> Result<(), BootstrapError>,
    ) -> Result<(), BootstrapError> {
        self.blocker.block_on(Box::pin(self.scan_async(visit)))
    }

    fn fetch(&self, keys: &[RecordKey]) -> Result<Vec<ImageRecord>, BootstrapError> {
        self.blocker.block_on(Box::pin(self.fetch_async(keys)))
    }
}

/// A snapshot failure as the bootstrap sees it: the stable code, and the part's object name when
/// the error names one. Never an image, an entity or an id.
fn source_failed(error: SnapshotError) -> BootstrapError {
    let reason = match &error {
        SnapshotError::PartInvalid { part, .. } => format!("{} (part {part})", error.code()),
        other => other.code().to_owned(),
    };
    BootstrapError::SourceFailed { reason }
}
