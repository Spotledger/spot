//! The relay seam: what the driver needs from the relay, so tests can fail it on demand.

use spot_events_contract::envelope::Position;
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

/// The relay as the driver uses it.
#[allow(async_fn_in_trait)]
pub trait ChunkRelay {
    /// The generation the relay writes into.
    fn generation(&self) -> u64;
    /// The sequence number the next entry must have.
    fn next_seq(&self) -> u64;
    /// Ships one batch of consecutive entries under `batch_id`.
    async fn ship(
        &mut self,
        batch_id: u64,
        entries: &[ShipEntry],
        now_millis: u64,
    ) -> Result<Shipped, ShipError>;
    /// Reads the store and repairs what a failed call left; success means the store is
    /// reachable and [`ChunkRelay::next_seq`] is true again.
    async fn resume(&mut self) -> Result<(), ShipError>;
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
