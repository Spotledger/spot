//! The relay seam: what the driver needs from the relay, so tests can fail it on demand.

use std::future::Future;

use spot_events_contract::envelope::Position;
use spot_journal_relay::tail::{BATCH_LINE_BUDGET, MAX_CHUNK_BYTES};
use spot_journal_relay::{Appended, Batch, Relay, RelayError};
use spot_journal_store::{JournalStore, StoreError};

/// One entry the driver ships: its sequence number and its envelope line (opaque).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipEntry {
    /// The sequence number within the relay's generation.
    pub seq: u64,
    /// The envelope as one line of UTF-8.
    pub line: Vec<u8>,
}

/// What a successful shipment promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shipped {
    /// The batch is durable in the object store up to `up_to_seq`.
    Durable {
        /// The last sequence number the batch holds.
        up_to_seq: u64,
    },
    /// The relay holds the batch but wrote nothing (`local`, `replica-sync`).
    Buffered,
}

/// Why a shipment failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShipError {
    /// The store failed or the relay needs a resume: a later attempt may succeed.
    StoreDown {
        /// The relay's stable error code.
        code: &'static str,
    },
    /// A failure a retry cannot cure (another writer, a broken chain, a bad batch).
    Stopped {
        /// The relay's stable error code.
        code: &'static str,
    },
}

/// How big one batch may be: the limits of the tail chunk the relay writes.
///
/// The driver never ships more than this in one call and never queues an entry that alone would
/// make a chunk the relay must refuse. The numbers are the relay's own (`tail` constants), not
/// copies; a test relay may return smaller ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLimits {
    /// The most line bytes (the sum over a batch's entries) the driver puts in one batch, unless
    /// one entry alone is larger (it then ships alone).
    pub line_budget: u64,
    /// The most bytes the encoded chunk of one batch may reach; the driver judges it with the
    /// relay's size bound (`tail::encoded_len_upper_bound_for`).
    pub max_chunk_bytes: u64,
}

impl BatchLimits {
    /// The limits of the version 2 tail chunk: [`BATCH_LINE_BUDGET`] and [`MAX_CHUNK_BYTES`].
    #[must_use]
    pub fn tail_chunk() -> Self {
        Self {
            line_budget: BATCH_LINE_BUDGET,
            max_chunk_bytes: MAX_CHUNK_BYTES,
        }
    }
}

/// The relay as the driver uses it.
///
/// The futures are `Send` so that [`crate::CommitDriver::run`] can be spawned on a multi-threaded
/// runtime. An implementation may still be written with `async fn`.
pub trait ChunkRelay: Send {
    /// The generation the relay writes into.
    fn generation(&self) -> u64;
    /// The sequence number the next entry must have.
    fn next_seq(&self) -> u64;
    /// Ships one batch of consecutive entries under `batch_id`.
    fn ship(
        &mut self,
        batch_id: u64,
        entries: &[ShipEntry],
        now_millis: u64,
    ) -> impl Future<Output = Result<Shipped, ShipError>> + Send;
    /// The size limits of one batch; read once when the driver is built. The default is the
    /// version 2 tail chunk's.
    fn batch_limits(&self) -> BatchLimits {
        BatchLimits::tail_chunk()
    }
    /// Reads the store and repairs what a failed call left; success means the store is
    /// reachable and [`ChunkRelay::next_seq`] is true again.
    fn resume(&mut self) -> impl Future<Output = Result<(), ShipError>> + Send;
}

fn classify(error: &RelayError) -> ShipError {
    let code = error.code();
    match error {
        RelayError::Store(StoreError::Io { .. } | StoreError::Backend { .. })
        | RelayError::ResumeRequired => ShipError::StoreDown { code },
        _ => ShipError::Stopped { code },
    }
}

fn millis_i64(millis: u64) -> i64 {
    i64::try_from(millis).unwrap_or(i64::MAX)
}

impl<S: JournalStore> ChunkRelay for Relay<S> {
    fn generation(&self) -> u64 {
        Relay::generation(self)
    }

    fn next_seq(&self) -> u64 {
        self.next_position().seq
    }

    async fn ship(
        &mut self,
        batch_id: u64,
        entries: &[ShipEntry],
        now_millis: u64,
    ) -> Result<Shipped, ShipError> {
        let generation = Relay::generation(self);
        let batch = Batch {
            batch_id,
            entries: entries
                .iter()
                .map(|e| {
                    (
                        Position {
                            generation,
                            seq: e.seq,
                        },
                        e.line.clone(),
                    )
                })
                .collect(),
        };
        match self
            .append_batch(batch, millis_i64(now_millis))
            .await
            .map_err(|e| classify(&e))?
        {
            Appended::ChunkDurable { up_to_seq, .. } => Ok(Shipped::Durable { up_to_seq }),
            Appended::Buffered { .. } => Ok(Shipped::Buffered),
        }
    }

    async fn resume(&mut self) -> Result<(), ShipError> {
        Relay::resume(self).await.map_err(|e| classify(&e))
    }
}
