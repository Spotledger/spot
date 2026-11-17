//! The hub-side director: routes a returning node, bounds parallel bootstraps and keeps
//! per-node progress that survives a restart.
//!
//! # The routes (ADR-066)
//!
//! [`spot_sync_catchup::choose_mode`] decides from the gap: a small gap with the applied prefix
//! still in hot retention stays a **live relay** (`Incremental`); a large one is **coalesced**
//! (the latest image per record, dependency order, mandatory lane first: the planner's `plan`
//! builds the bundles); a new or resynced node, a prefix older than the hot queue whose coalesced
//! plan would cost more than the configured share of a snapshot, or a gap past the threshold with
//! that cost, takes a **snapshot bootstrap**.
//!
//! A bootstrap needs a slot: at most `max_parallel_bootstraps` run at once per tenant; the others
//! are told [`Route::Wait`] with a retry-after (a base plus a spread the node id picks, so 100
//! nodes do not come back in the same millisecond). Each chunk spends one of the node's tokens.
//!
//! # Resumable
//!
//! Progress (mode, chunks planned, chunks applied) is stored through a [`ProgressStore`] on every
//! change, so a restart of the hub resumes a bootstrap at the chunk after the last applied one and
//! never from zero. A chunk reported out of order is refused.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use spot_sync_bootstrap::{Admission, Decision};
use spot_sync_catchup::{GapSummary, Mode, ModeReason, Settings, choose_mode};

use crate::config::CatchupConfig;
use crate::error::HostError;

/// What a node is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// A coalesced, dependency-ordered catch-up.
    CatchUp,
    /// A snapshot bootstrap.
    Bootstrap,
}

/// One node's stored progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// What it is doing.
    pub phase: Phase,
    /// Chunks planned (0 until the plan is cut).
    pub total_chunks: u64,
    /// Chunks applied, in order: the next chunk to send is this index.
    pub done_chunks: u64,
}

/// Where progress is kept (the hub's durable state in production, memory in a test).
pub trait ProgressStore: Send + Sync {
    /// The node's progress, if it has any.
    ///
    /// # Errors
    /// [`HostError::Store`].
    fn load(&self, node: &str) -> Result<Option<Progress>, HostError>;
    /// Stores the node's progress.
    ///
    /// # Errors
    /// [`HostError::Store`].
    fn save(&self, node: &str, progress: &Progress) -> Result<(), HostError>;
    /// Forgets the node's progress.
    ///
    /// # Errors
    /// [`HostError::Store`].
    fn clear(&self, node: &str) -> Result<(), HostError>;
}

/// A progress store in memory.
#[derive(Default)]
pub struct MemoryProgress(Mutex<BTreeMap<String, Progress>>);

impl MemoryProgress {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Progress>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ProgressStore for MemoryProgress {
    fn load(&self, node: &str) -> Result<Option<Progress>, HostError> {
        Ok(self.lock().get(node).copied())
    }

    fn save(&self, node: &str, progress: &Progress) -> Result<(), HostError> {
        self.lock().insert(node.to_owned(), *progress);
        Ok(())
    }

    fn clear(&self, node: &str) -> Result<(), HostError> {
        self.lock().remove(node);
        Ok(())
    }
}

/// What the hub does for a node now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Relay live: the node is close behind.
    Live,
    /// Serve the coalesced, dependency-ordered plan; the reasons are the planner's.
    CatchUp {
        /// The rules that fired, in order.
        reasons: Vec<ModeReason>,
        /// Chunks already applied (0 for a new plan).
        resume_from_chunk: u64,
    },
    /// Serve the snapshot bootstrap from this chunk, then the stream from P+1.
    Bootstrap {
        /// The next chunk to send.
        resume_from_chunk: u64,
    },
    /// No bootstrap slot (or no chunk token) now: ask again after this many milliseconds.
    Wait {
        /// Milliseconds to wait.
        retry_after_ms: u64,
    },
}

/// The answer to "chunk `index` is applied".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkAck {
    /// Recorded; send the next one.
    Next,
    /// Recorded, but the node is out of chunk tokens: wait this long before the next.
    Wait(u64),
    /// That was the last chunk: the bootstrap is complete and the stream continues from P+1.
    Complete,
}

/// The director of one hub (one tenant).
pub struct Director {
    settings: Settings,
    admission: Mutex<Admission>,
    store: Arc<dyn ProgressStore>,
}

impl Director {
    /// A director over `store`.
    ///
    /// # Errors
    /// [`HostError`] for an out-of-bounds configuration.
    pub fn new(config: &CatchupConfig, store: Arc<dyn ProgressStore>) -> Result<Self, HostError> {
        let settings = config.planner()?;
        let admission =
            Admission::new(config.admission()).map_err(|e| HostError::Planner(e.code()))?;
        Ok(Self {
            settings,
            admission: Mutex::new(admission),
            store,
        })
    }

    fn admission(&self) -> MutexGuard<'_, Admission> {
        self.admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Routes `node` given the gap `summary` at `now_ms`.
    ///
    /// # Errors
    /// [`HostError::Planner`] for a summary that cannot describe a gap, [`HostError::Store`].
    pub fn route(&self, node: &str, summary: &GapSummary, now_ms: i64) -> Result<Route, HostError> {
        if let Some(progress) = self.store.load(node)? {
            // A stored plan resumes whatever the summary now says: never from zero.
            return match progress.phase {
                Phase::CatchUp => Ok(Route::CatchUp {
                    reasons: Vec::new(),
                    resume_from_chunk: progress.done_chunks,
                }),
                Phase::Bootstrap => Ok(self.bootstrap(node, progress.done_chunks, now_ms)),
            };
        }
        let decision =
            choose_mode(summary, &self.settings).map_err(|e| HostError::Planner(e.code()))?;
        match decision.mode {
            Mode::Incremental => Ok(Route::Live),
            Mode::Coalesced => {
                self.store.save(
                    node,
                    &Progress {
                        phase: Phase::CatchUp,
                        total_chunks: 0,
                        done_chunks: 0,
                    },
                )?;
                Ok(Route::CatchUp {
                    reasons: decision.reasons,
                    resume_from_chunk: 0,
                })
            }
            Mode::Snapshot => {
                let route = self.bootstrap(node, 0, now_ms);
                if matches!(route, Route::Bootstrap { .. }) {
                    self.store.save(
                        node,
                        &Progress {
                            phase: Phase::Bootstrap,
                            total_chunks: 0,
                            done_chunks: 0,
                        },
                    )?;
                }
                Ok(route)
            }
        }
    }

    fn bootstrap(&self, node: &str, resume_from_chunk: u64, now_ms: i64) -> Route {
        match self.admission().admit(node, now_ms) {
            Decision::Admit => Route::Bootstrap { resume_from_chunk },
            Decision::RetryAfter(retry_after_ms) => Route::Wait { retry_after_ms },
        }
    }

    /// Records how many chunks the plan was cut into.
    ///
    /// # Errors
    /// [`HostError::ChunkOutOfOrder`] when the node has no progress, [`HostError::Store`].
    pub fn plan_cut(&self, node: &str, total_chunks: u64) -> Result<(), HostError> {
        let mut progress = self.store.load(node)?.ok_or(HostError::ChunkOutOfOrder)?;
        progress.total_chunks = total_chunks;
        self.store.save(node, &progress)
    }

    /// Records that chunk `index` of the node's plan was applied, checked against the hash by the
    /// caller (a chunk whose hash does not match the index is refused before this).
    ///
    /// # Errors
    /// [`HostError::ChunkOutOfOrder`] unless `index` is the next chunk, [`HostError::Store`].
    pub fn chunk_done(&self, node: &str, index: u64, now_ms: i64) -> Result<ChunkAck, HostError> {
        let mut progress = self.store.load(node)?.ok_or(HostError::ChunkOutOfOrder)?;
        if index != progress.done_chunks {
            return Err(HostError::ChunkOutOfOrder);
        }
        progress.done_chunks = index.saturating_add(1);
        self.store.save(node, &progress)?;
        if progress.total_chunks > 0 && progress.done_chunks >= progress.total_chunks {
            return Ok(ChunkAck::Complete);
        }
        if progress.phase == Phase::Bootstrap {
            return match self.admission().chunk(node, now_ms) {
                Ok(Decision::Admit) => Ok(ChunkAck::Next),
                Ok(Decision::RetryAfter(ms)) => Ok(ChunkAck::Wait(ms)),
                Err(e) => Err(HostError::Planner(e.code())),
            };
        }
        Ok(ChunkAck::Next)
    }

    /// The node is live again (or gave up): its progress is forgotten and its slot freed.
    ///
    /// # Errors
    /// [`HostError::Store`].
    pub fn finish(&self, node: &str) -> Result<(), HostError> {
        self.store.clear(node)?;
        self.admission().release(node);
        Ok(())
    }

    /// The queue's word for the node: `live`, `catching_up 40% (chunk 4 of 10)`,
    /// `bootstrapping 3 of 10`.
    ///
    /// # Errors
    /// [`HostError::Store`].
    pub fn status_text(&self, node: &str) -> Result<String, HostError> {
        let Some(progress) = self.store.load(node)? else {
            return Ok("live".to_owned());
        };
        let word = match progress.phase {
            Phase::CatchUp => "catching_up",
            Phase::Bootstrap => "bootstrapping",
        };
        if progress.total_chunks == 0 {
            return Ok(format!("{word} (planning)"));
        }
        let percent = progress.done_chunks.saturating_mul(100) / progress.total_chunks;
        Ok(format!(
            "{word} {percent}% (chunk {} of {})",
            progress.done_chunks, progress.total_chunks
        ))
    }
}
