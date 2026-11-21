//! The durable checkpoint: the outbox position the projection has processed.
//!
//! The projector reads the position it last processed from a [`CheckpointStore`] before each
//! record and writes it back after the sink accepted the record, so a restart resumes where it
//! stopped and a redelivered record is a no-op. The store is a seam: the reference
//! [`InMemoryCheckpointStore`] is for tests and the single-node host, and a backend table
//! implements the same three methods later.

use std::collections::BTreeMap;
use std::sync::RwLock;

use spot_graph_index::GraphKey;
use spot_storage_contract::FeedPosition;

use crate::error::ProjectionError;

/// Where the projection's progress is kept, per graph.
pub trait CheckpointStore: Send + Sync {
    /// The last processed position, or `None` before the first record.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Checkpoint`] when the store cannot answer.
    fn load(&self, key: &GraphKey) -> Result<Option<FeedPosition>, ProjectionError>;

    /// Records `position` as processed.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Checkpoint`] when the store cannot write.
    fn save(&self, key: &GraphKey, position: FeedPosition) -> Result<(), ProjectionError>;

    /// Forgets the checkpoint, as a rebuild does.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Checkpoint`] when the store cannot clear.
    fn clear(&self, key: &GraphKey) -> Result<(), ProjectionError>;
}

/// A checkpoint store in memory, for tests and the single-node reference host.
#[derive(Debug, Default)]
pub struct InMemoryCheckpointStore {
    positions: RwLock<BTreeMap<GraphKey, FeedPosition>>,
}

impl InMemoryCheckpointStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<GraphKey, FeedPosition>> {
        self.positions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<GraphKey, FeedPosition>> {
        self.positions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CheckpointStore for InMemoryCheckpointStore {
    fn load(&self, key: &GraphKey) -> Result<Option<FeedPosition>, ProjectionError> {
        Ok(self.read().get(key).copied())
    }

    fn save(&self, key: &GraphKey, position: FeedPosition) -> Result<(), ProjectionError> {
        self.write().insert(key.clone(), position);
        Ok(())
    }

    fn clear(&self, key: &GraphKey) -> Result<(), ProjectionError> {
        self.write().remove(key);
        Ok(())
    }
}
