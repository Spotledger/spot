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
//! - **Generation** is the part's own counter of rebuilds requested since the process started: it
//!   starts at 0 and one rebuild bumps it by exactly 1. `restore` puts the previous generation back
//!   only when nothing newer ran (otherwise it refuses: "a newer rebuild ran"). The queued backfill
//!   itself cannot be withdrawn (the port cannot cancel); it is idempotent, so a restored record
//!   with a replay still running is harmless.
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
use std::pin::Pin;
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

struct State {
    generation: u64,
    requested_at: Option<i64>,
}

/// The retrieval-index part.
pub struct RetrievalIndexPart {
    lag: Arc<dyn IndexLag>,
    submit: Arc<dyn JobSubmit>,
    queue: String,
    clock: Arc<dyn Clock>,
    handle: Handle,
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
            state: Mutex::new(State {
                generation: 0,
                requested_at: None,
            }),
        }
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
            generation: state.generation,
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

/// The retrieval-index part for this site, or `None` when the retrieval service is not provided
/// (it is off by default, per tenant) or the job-submit port is missing.
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
    from_parts(
        Some(lag),
        ctx.port::<dyn JobSubmit>(),
        env.loaded.common.services.jobs.queues.first().cloned(),
        Arc::clone(ctx.clock()),
        capture_runtime(),
    )
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
        let is_new = self
            .part
            .handle
            .block_on(self.part.submit.submit(spec))
            .map_err(|_| OpError::new(op, "the job queue did not take the backfill"))?;
        if !is_new {
            return Err(OpError::new(op, "a backfill is already queued"));
        }
        let mut state = self.part.lock();
        let before_generation = state.generation;
        state.generation = state.generation.saturating_add(1);
        state.requested_at = Some(self.part.now());
        Ok(IndexChange {
            name: name.clone(),
            before: before.health,
            after: IndexHealth::Fresh,
            before_generation,
            after_generation: state.generation,
        })
    }

    fn restore(&mut self, change: &IndexChange) -> Result<(), OpError> {
        let op = "index_restore";
        if change.name.as_str() != INDEX_NAME {
            return Err(no_index(op, &change.name));
        }
        let mut state = self.part.lock();
        if state.generation != change.after_generation {
            return Err(OpError::new(op, "a newer rebuild ran"));
        }
        state.generation = change.before_generation;
        state.requested_at = None;
        Ok(())
    }
}
