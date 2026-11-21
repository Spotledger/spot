//! The retrieval-index part of the world (WP-AI-07.T6): the real [`IndexOps`] over the retrieval
//! service, through what the platform exposes of it.
//!
//! The site has one index, `retrieval`. The retrieval service provides no state port (only its
//! tool), so the part reads the two public facts a backfill is about and uses the public
//! operation the service gives for rebuilding:
//!
//! - **Staleness** is "the indexer's cursor is behind the change feed head": the part reads this
//!   node's cursor file through the retrieval module's own [`FilePositions`] and asks the feed
//!   whether any change follows it ([`IndexLag`]; the feed read is the one the indexer makes).
//!   The cursor is node-local, so this is this node's view, as the indexer's own is. A feed that
//!   cannot be read counts as fresh (the part never reports staleness it did not see).
//! - **Rebuild** enqueues the backfill job `platform:retrieval.backfill` through the `JobSubmit`
//!   port. The job's handler is what resets the cursor and replays the feed on the node that holds
//!   the indexer lease; the part does not write the cursor itself, so it never races the indexer. One
//!   backfill is in flight at a time: a rebuild while one is queued is **refused** ("a backfill is
//!   already queued"), never queued twice (the job's idempotency key coalesces the duplicate, and
//!   the part turns that into the refusal).
//! - **Generation** is the part's own counter of rebuilds requested: it starts at 0 and one rebuild
//!   bumps it by exactly 1. On a site it is kept in a small file beside the retrieval cursor
//!   (`sites/<site>/retrieval.generation`, one decimal, written to a temporary file and renamed, as
//!   the cursor is), so it **survives a restart** and a restore after a restart still compares
//!   against the counter it was made from. The file is node-local, like the cursor, and is not the
//!   feed generation the cursor carries (a different notion). A rebuild writes `generation + 1`
//!   **before** it enqueues the backfill, and puts the old value back if the queue refuses the job.
//!   `restore` puts the previous generation back only when nothing newer ran (otherwise it
//!   refuses: "a newer rebuild ran"). The queued backfill itself cannot be withdrawn (the port
//!   cannot cancel); it is idempotent, so a restored record with a replay still running is
//!   harmless. A part built without a [`GenerationStore`] keeps the counter in memory (tests).
//! - **Health** is *stale* when the cursor is behind and no backfill of this part is in flight, and
//!   *fresh* otherwise: a rebuild just requested reads fresh at the new generation (nothing more
//!   for an operator to do), and it reads stale again if the cursor is still behind after
//!   [`REQUEST_WINDOW_SECS`] (the backfill did not run), measured by the injected clock.
//! - `rehearsal` snapshots health and generation into the in-memory fixture; it never enqueues.
//!
//! If the retrieval service is not provided on the site (it is off by default, per tenant), or the
//! `JobSubmit` port is missing, `from_site` answers `None`.
//!
//! The ops are synchronous and block on the runtime handle captured when the site was built
//! (`Handle::block_on`, never `block_in_place`).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use spot_core::Clock;
use spot_remedies::fixture::FakeIndex;
use spot_remedies::{IndexChange, IndexHealth, IndexOps, IndexRef, IndexView, OpError};
use spot_retrieval::PositionStore;
use spot_storage_contract::{ChangeFeed, FeedPosition};
use tokio::runtime::Handle;

use super::parts::{IndexPart, capture_runtime};
use crate::services::SiteCtx;
use crate::services::ports::{JobSpec, JobSubmit};
use crate::services::remedies::world::WorldError;
use crate::services::retrieval::BACKFILL_JOB;
use crate::services::retrieval::ports::FilePositions;
use crate::services::retrieval::tool::RetrieveTool;

/// The one index of a site.
pub const INDEX_NAME: &str = "retrieval";

/// How long a requested backfill counts as in flight, in seconds, before a cursor that is still
/// behind reads stale again.
pub const REQUEST_WINDOW_SECS: i64 = 3_600;

/// The idempotency key of the backfill job this part enqueues (one backfill at a time).
const BACKFILL_KEY: &str = "platform:retrieval.backfill:remedy";

/// A boxed future the lag read returns.
pub type LagFuture<'a> = Pin<Box<dyn Future<Output = Option<bool>> + Send + 'a>>;

/// Whether the indexer's cursor is behind the change feed head.
pub trait IndexLag: Send + Sync {
    /// `Some(true)` when a committed change follows the cursor, `Some(false)` when the cursor is
    /// at the head, `None` when it cannot be told.
    fn behind(&self) -> LagFuture<'_>;
}

/// [`IndexLag`] over the site's change feed and this node's retrieval cursor.
pub struct FeedLag<F: ChangeFeed> {
    feed: F,
    cursor: FilePositions,
}

impl<F: ChangeFeed> FeedLag<F> {
    /// A lag read over `feed` and the cursor file `cursor`.
    #[must_use]
    pub fn new(feed: F, cursor: FilePositions) -> Self {
        Self { feed, cursor }
    }
}

impl<F: ChangeFeed + 'static> IndexLag for FeedLag<F> {
    fn behind(&self) -> LagFuture<'_> {
        Box::pin(async move {
            let after = self.cursor.load().unwrap_or(FeedPosition::START);
            match self.feed.read_changes(after, 1).await {
                Ok(changes) => Some(!changes.is_empty()),
                Err(_) => None,
            }
        })
    }
}

/// Where the index generation is kept.
pub trait GenerationStore: Send + Sync {
    /// The stored generation; 0 when none was ever stored or it cannot be read.
    fn load(&self) -> u64;

    /// Stores `generation` so a later [`load`](Self::load), also after a restart, returns it.
    ///
    /// # Errors
    ///
    /// `Err(())` when the value could not be stored; nothing was changed.
    #[allow(clippy::result_unit_err)]
    fn save(&self, generation: u64) -> Result<(), ()>;
}

/// A generation kept in memory only (lost at restart): what a part gets unless it is given a
/// durable store.
#[derive(Debug, Default)]
pub struct MemoryGeneration(AtomicU64);

impl GenerationStore for MemoryGeneration {
    fn load(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn save(&self, generation: u64) -> Result<(), ()> {
        self.0.store(generation, Ordering::SeqCst);
        Ok(())
    }
}

/// A generation kept in a small file: a single decimal, replaced through a temporary file and a
/// rename, so a reader sees the old value or the new one, never half of one. Node-local.
#[derive(Debug)]
pub struct FileGeneration {
    path: PathBuf,
}

impl FileGeneration {
    /// A generation in `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl GenerationStore for FileGeneration {
    fn load(&self) -> u64 {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    fn save(&self, generation: u64) -> Result<(), ()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| ())?;
        }
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        std::fs::write(&temporary, generation.to_string()).map_err(|_| ())?;
        std::fs::rename(&temporary, &self.path).map_err(|_| ())
    }
}

struct State {
    requested_at: Option<i64>,
}

/// The retrieval-index part.
pub struct RetrievalIndexPart {
    lag: Arc<dyn IndexLag>,
    submit: Arc<dyn JobSubmit>,
    queue: String,
    clock: Arc<dyn Clock>,
    handle: Handle,
    generations: Arc<dyn GenerationStore>,
    state: Mutex<State>,
}

impl RetrievalIndexPart {
    /// A part reading `lag`, enqueueing the backfill on `queue` through `submit`. `clock` is the
    /// injected clock; `handle` is the runtime the ops block on.
    #[must_use]
    pub fn new(
        lag: Arc<dyn IndexLag>,
        submit: Arc<dyn JobSubmit>,
        queue: String,
        clock: Arc<dyn Clock>,
        handle: Handle,
    ) -> Self {
        Self {
            lag,
            submit,
            queue,
            clock,
            handle,
            generations: Arc::new(MemoryGeneration::default()),
            state: Mutex::new(State { requested_at: None }),
        }
    }

    /// Keeps the generation in `generations` (a [`FileGeneration`] on a site) instead of in memory.
    #[must_use]
    pub fn with_generations(mut self, generations: Arc<dyn GenerationStore>) -> Self {
        self.generations = generations;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn in_flight(&self, state: &State) -> bool {
        state
            .requested_at
            .is_some_and(|at| self.now().saturating_sub(at) < REQUEST_WINDOW_SECS)
    }

    /// The index as the ports see it now.
    fn view(&self) -> IndexView {
        let behind = self.handle.block_on(self.lag.behind());
        let mut state = self.lock();
        if behind == Some(false) {
            state.requested_at = None;
        }
        let stale = behind == Some(true) && !self.in_flight(&state);
        IndexView {
            name: IndexRef::new(INDEX_NAME),
            health: if stale {
                IndexHealth::Stale
            } else {
                IndexHealth::Fresh
            },
            generation: self.generations.load(),
        }
    }
}

/// The part, or `None` when the retrieval side is missing (no lag read, no `JobSubmit`, no queue,
/// or no runtime to block on).
#[must_use]
pub fn from_parts(
    lag: Option<Arc<dyn IndexLag>>,
    submit: Option<Arc<dyn JobSubmit>>,
    queue: Option<String>,
    clock: Arc<dyn Clock>,
    handle: Option<Handle>,
) -> Option<Arc<dyn IndexPart>> {
    Some(Arc::new(RetrievalIndexPart::new(
        lag?, submit?, queue?, clock, handle?,
    )))
}

/// The generation file of a site's index: `<root>/sites/<site>/retrieval.generation`, beside the
/// retrieval cursor (`retrieval.cursor`).
#[must_use]
pub fn generation_path(root: &Path, site: &str) -> PathBuf {
    root.join("sites")
        .join(site)
        .join("retrieval")
        .with_extension("generation")
}

/// The retrieval-index part for this site, or `None` when the retrieval service is not provided
/// (it is off by default, per tenant) or the job-submit port is missing. Its generation lives in
/// the site's generation file, so it survives a restart.
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn IndexPart>> {
    let env = *ctx.env().ok()?;
    // The retrieval module provides its tool only when it is on for this site and fully built.
    ctx.port::<RetrieveTool>()?;
    let cursor = FilePositions::new(
        env.root
            .join("sites")
            .join(&env.site.site)
            .join("retrieval.cursor"),
    );
    let lag: Arc<dyn IndexLag> = Arc::new(FeedLag::new(env.backend.clone(), cursor));
    let part = RetrievalIndexPart::new(
        lag,
        ctx.port::<dyn JobSubmit>()?,
        env.loaded.common.services.jobs.queues.first().cloned()?,
        Arc::clone(ctx.clock()),
        capture_runtime()?,
    )
    .with_generations(Arc::new(FileGeneration::new(generation_path(
        env.root,
        &env.site.site,
    ))));
    Some(Arc::new(part))
}

impl IndexPart for RetrievalIndexPart {
    fn live(&self) -> Box<dyn IndexOps + '_> {
        Box::new(RetrievalIndexOps { part: self })
    }

    fn rehearsal(&self) -> Result<Box<dyn IndexOps>, WorldError> {
        let view = self.view();
        let mut fake = FakeIndex::new();
        fake.seed(INDEX_NAME, view.health, view.generation);
        Ok(Box::new(fake))
    }
}

/// The live ops.
struct RetrievalIndexOps<'a> {
    part: &'a RetrievalIndexPart,
}

fn no_index(op: &str, name: &IndexRef) -> OpError {
    OpError::new(op, &format!("no index `{}`", name.as_str()))
}

impl IndexOps for RetrievalIndexOps<'_> {
    fn stale(&self) -> Vec<IndexRef> {
        let view = self.part.view();
        if view.health == IndexHealth::Stale {
            vec![view.name]
        } else {
            Vec::new()
        }
    }

    fn index(&self, name: &IndexRef) -> Option<IndexView> {
        (name.as_str() == INDEX_NAME).then(|| self.part.view())
    }

    fn rebuild(&mut self, name: &IndexRef) -> Result<IndexChange, OpError> {
        let op = "index_rebuild";
        if name.as_str() != INDEX_NAME {
            return Err(no_index(op, name));
        }
        let before = self.part.view();
        let spec = JobSpec {
            name: BACKFILL_JOB.to_owned(),
            queue: self.part.queue.clone(),
            args: serde_json::json!({}),
            idempotency_key: BACKFILL_KEY.to_owned(),
            not_before: None,
        };
        // One lock from the read of the generation to the enqueue: two rebuilds in this process
        // cannot both take the same `generation + 1`.
        let mut state = self.part.lock();
        let before_generation = self.part.generations.load();
        let after_generation = before_generation.saturating_add(1);
        // The new generation is written first. If the queue then refuses the job, the old value is
        // put back, so a refused rebuild leaves the generation where it was.
        self.part
            .generations
            .save(after_generation)
            .map_err(|()| OpError::new(op, "the index generation could not be saved"))?;
        let outcome = self.part.handle.block_on(self.part.submit.submit(spec));
        match outcome {
            Ok(true) => {}
            Ok(false) => {
                let _ = self.part.generations.save(before_generation);
                return Err(OpError::new(op, "a backfill is already queued"));
            }
            Err(_) => {
                let _ = self.part.generations.save(before_generation);
                return Err(OpError::new(op, "the job queue did not take the backfill"));
            }
        }
        state.requested_at = Some(self.part.now());
        Ok(IndexChange {
            name: name.clone(),
            before: before.health,
            after: IndexHealth::Fresh,
            before_generation,
            after_generation,
        })
    }

    fn restore(&mut self, change: &IndexChange) -> Result<(), OpError> {
        let op = "index_restore";
        if change.name.as_str() != INDEX_NAME {
            return Err(no_index(op, &change.name));
        }
        let mut state = self.part.lock();
        if self.part.generations.load() != change.after_generation {
            return Err(OpError::new(op, "a newer rebuild ran"));
        }
        self.part
            .generations
            .save(change.before_generation)
            .map_err(|()| OpError::new(op, "the index generation could not be saved"))?;
        state.requested_at = None;
        Ok(())
    }
}
