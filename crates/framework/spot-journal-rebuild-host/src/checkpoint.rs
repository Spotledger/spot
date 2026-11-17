//! The durable checkpoint and the `rebuilding` state flag.
//!
//! A rebuild writes a checkpoint `(generation, seq)` every N images and at the end. An
//! interrupted rebuild resumes from it (never from zero, never twice: the sink skips what the
//! checkpoint covers, and the apply path is idempotent by name and version) and **refuses to
//! resume if the plan changed** (the checkpoint carries the hash of its plan). While the state is
//! [`State::Rebuilding`] the site refuses to start serving ([`crate::HostError::Incomplete`]):
//! finish the rebuild or discard the target.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::error::HostError;

/// A boxed future, so the traits are object safe on any executor.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, HostError>> + Send + 'a>>;

/// Whether the database is whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// A rebuild started and has not finished: the site must not serve this database.
    Rebuilding,
    /// The rebuild finished.
    Complete,
}

/// The saved progress of one site's rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The hash of the plan this progress belongs to.
    pub plan_hash: String,
    /// The generation of the last position applied.
    pub generation: u64,
    /// The sequence number of the last position applied (0: nothing applied yet).
    pub seq: u64,
    /// Whether the rebuild finished.
    pub state: State,
}

/// Where checkpoints live (the rebuilt database itself, so a restore of a half-built copy
/// carries its own flag).
pub trait CheckpointStore: Send + Sync {
    /// The site's checkpoint, if any.
    fn load<'a>(&'a self, site: &'a str) -> Fut<'a, Option<Checkpoint>>;
    /// Saves the site's checkpoint, replacing the previous one.
    fn save<'a>(&'a self, site: &'a str, checkpoint: &'a Checkpoint) -> Fut<'a, ()>;
    /// Forgets the site's checkpoint: an explicit discard of a half-built target.
    fn discard<'a>(&'a self, site: &'a str) -> Fut<'a, ()>;
}

/// An in-memory store for tests and dry runs.
#[derive(Default)]
pub struct MemoryCheckpoints(Mutex<Vec<(String, Checkpoint)>>);

impl MemoryCheckpoints {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CheckpointStore for MemoryCheckpoints {
    fn load<'a>(&'a self, site: &'a str) -> Fut<'a, Option<Checkpoint>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .find(|(s, _)| s == site)
                .map(|(_, c)| c.clone()))
        })
    }

    fn save<'a>(&'a self, site: &'a str, checkpoint: &'a Checkpoint) -> Fut<'a, ()> {
        Box::pin(async move {
            let mut all = self.0.lock().unwrap_or_else(|p| p.into_inner());
            all.retain(|(s, _)| s != site);
            all.push((site.to_owned(), checkpoint.clone()));
            Ok(())
        })
    }

    fn discard<'a>(&'a self, site: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(s, _)| s != site);
            Ok(())
        })
    }
}

/// The PostgreSQL checkpoint table (created by this offline tool in the target database before
/// the first write; the site is stopped, like `spot migrate`).
pub const DDL: &str = "CREATE TABLE IF NOT EXISTS spot_journal_rebuild_state (\
    site text PRIMARY KEY, plan_hash text NOT NULL, generation bigint NOT NULL, \
    seq bigint NOT NULL, complete boolean NOT NULL)";

/// The checkpoint store over a database.
pub struct PgCheckpoints {
    backend: spot_postgres::PgBackend,
}

impl PgCheckpoints {
    /// A store over the rebuilt database's pool.
    #[must_use]
    pub fn new(backend: spot_postgres::PgBackend) -> Self {
        Self { backend }
    }

    /// Creates the table (idempotent).
    ///
    /// # Errors
    /// [`HostError::CheckpointFailed`].
    pub async fn prepare(&self) -> Result<(), HostError> {
        let client = self
            .backend
            .pool()
            .get()
            .await
            .map_err(|_| HostError::CheckpointFailed)?;
        client
            .batch_execute(DDL)
            .await
            .map_err(|_| HostError::CheckpointFailed)
    }
}

impl CheckpointStore for PgCheckpoints {
    fn load<'a>(&'a self, site: &'a str) -> Fut<'a, Option<Checkpoint>> {
        Box::pin(async move {
            let client = self
                .backend
                .pool()
                .get()
                .await
                .map_err(|_| HostError::CheckpointFailed)?;
            let rows = client
                .query(
                    "SELECT plan_hash, generation, seq, complete FROM spot_journal_rebuild_state WHERE site = $1",
                    &[&site],
                )
                .await
                .map_err(|_| HostError::CheckpointFailed)?;
            let Some(row) = rows.first() else {
                return Ok(None);
            };
            let generation: i64 = row.try_get(1).map_err(|_| HostError::CheckpointFailed)?;
            let seq: i64 = row.try_get(2).map_err(|_| HostError::CheckpointFailed)?;
            let complete: bool = row.try_get(3).map_err(|_| HostError::CheckpointFailed)?;
            Ok(Some(Checkpoint {
                plan_hash: row.try_get(0).map_err(|_| HostError::CheckpointFailed)?,
                generation: u64::try_from(generation).map_err(|_| HostError::CheckpointFailed)?,
                seq: u64::try_from(seq).map_err(|_| HostError::CheckpointFailed)?,
                state: if complete {
                    State::Complete
                } else {
                    State::Rebuilding
                },
            }))
        })
    }

    fn save<'a>(&'a self, site: &'a str, checkpoint: &'a Checkpoint) -> Fut<'a, ()> {
        Box::pin(async move {
            let client = self
                .backend
                .pool()
                .get()
                .await
                .map_err(|_| HostError::CheckpointFailed)?;
            let generation =
                i64::try_from(checkpoint.generation).map_err(|_| HostError::CheckpointFailed)?;
            let seq = i64::try_from(checkpoint.seq).map_err(|_| HostError::CheckpointFailed)?;
            let complete = checkpoint.state == State::Complete;
            client
                .execute(
                    "INSERT INTO spot_journal_rebuild_state (site, plan_hash, generation, seq, complete) \
                     VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (site) DO UPDATE SET plan_hash = EXCLUDED.plan_hash, \
                       generation = EXCLUDED.generation, seq = EXCLUDED.seq, complete = EXCLUDED.complete",
                    &[&site, &checkpoint.plan_hash, &generation, &seq, &complete],
                )
                .await
                .map_err(|_| HostError::CheckpointFailed)?;
            Ok(())
        })
    }

    fn discard<'a>(&'a self, site: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            let client = self
                .backend
                .pool()
                .get()
                .await
                .map_err(|_| HostError::CheckpointFailed)?;
            client
                .execute(
                    "DELETE FROM spot_journal_rebuild_state WHERE site = $1",
                    &[&site],
                )
                .await
                .map_err(|_| HostError::CheckpointFailed)?;
            Ok(())
        })
    }
}
