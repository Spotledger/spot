//! WP-AI-07.T6: the cache part (over an in-process `CacheStore`) and the retrieval-index part (over
//! a lag double and a recording `JobSubmit`) of the world. The ops block on the runtime handle, so
//! each runs on a blocking thread, as the remedy host calls them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cache::{Bytes, CacheStore, InProcessCache, InProcessConfig};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::retrieval::BACKFILL_JOB;
use spot_cli::services::world_access::cache::step_namespace;
use spot_cli::services::world_access::cache::{StoreCachePart, from_parts as cache_from_parts};
use spot_cli::services::world_access::index::{
    INDEX_NAME, IndexLag, LagFuture, REQUEST_WINDOW_SECS, RetrievalIndexPart,
    from_parts as index_from_parts,
};
use spot_cli::services::world_access::parts::{CachePart, IndexPart};
use spot_core::{Clock, ManualClock, SiteId};
use spot_remedies::{CacheNs, IndexChange, IndexHealth, IndexRef};
use time::OffsetDateTime;
use tokio::runtime::Handle;

fn site() -> SiteId {
    SiteId::new("site-1").unwrap()
}

fn bytes(text: &str) -> Bytes {
    Arc::from(text.as_bytes())
}

/// Runs `work` on a blocking thread, as the host does.
async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(work).await.unwrap()
}

// ---------------------------------------------------------------------------------------------
// cache
// ---------------------------------------------------------------------------------------------

async fn cache_world() -> (Arc<InProcessCache>, Arc<StoreCachePart>) {
    let store = Arc::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 20)));
    let site = site();
    for key in ["step:a", "step:b", "other:x"] {
        store.set(&site, key, bytes("v"), None).await.unwrap();
    }
    let part = Arc::new(StoreCachePart::new(
        Arc::clone(&store) as Arc<dyn CacheStore>,
        site,
        Handle::current(),
    ));
    (store, part)
}

fn step() -> CacheNs {
    CacheNs::new(step_namespace())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_step_namespace_is_known_and_counts_the_sites_live_entries() {
    let (_store, part) = cache_world().await;
    let (namespaces, step_entries, other_entries) = blocking(move || {
        let ops = part.live();
        (
            ops.namespaces(),
            ops.entries(&step()),
            ops.entries(&CacheNs::new("nothing")),
        )
    })
    .await;
    assert_eq!(namespaces, vec![step()]);
    assert_eq!(step_namespace(), "step");
    assert_eq!(step_entries, 3, "the store counts the site's live entries");
    assert_eq!(other_entries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flush_clears_the_one_namespace_and_leaves_the_others() {
    let (store, part) = cache_world().await;
    let change = blocking(move || part.live().flush(&step()).unwrap()).await;
    assert_eq!((change.before, change.after), (3, 1));
    let site = site();
    assert!(!store.exists(&site, "step:a").await);
    assert!(!store.exists(&site, "step:b").await);
    assert!(store.exists(&site, "other:x").await, "only the namespace");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_namespace_is_refused_and_nothing_is_flushed() {
    let (store, part) = cache_world().await;
    let refused = blocking(move || part.live().flush(&CacheNs::new("other")))
        .await
        .unwrap_err();
    assert!(refused.detail.contains("no namespace"), "{refused}");
    assert!(store.exists(&site(), "step:a").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_flush_records_the_reversal_and_restores_no_entries() {
    let (store, part) = cache_world().await;
    let (undone, unknown) = blocking(move || {
        let mut ops = part.live();
        let change = ops.flush(&step()).unwrap();
        let undone = ops.undo_flush(&change);
        let mut bad = change.clone();
        bad.ns = CacheNs::new("other");
        (undone, ops.undo_flush(&bad))
    })
    .await;
    assert!(undone.is_ok());
    assert!(unknown.is_err());
    assert!(
        !store.exists(&site(), "step:a").await,
        "a cache is derived data: nothing is put back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_flush_leaves_the_live_cache_alone() {
    let (store, part) = cache_world().await;
    let (before, after) = blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        let change = ops.flush(&step()).unwrap();
        (change.before, ops.entries(&step()))
    })
    .await;
    assert_eq!((before, after), (3, 0));
    let site = site();
    assert!(store.exists(&site, "step:a").await);
    assert_eq!(store.stats(&site).await.entries, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_part_with_no_store_is_none() {
    let store: Arc<dyn CacheStore> =
        Arc::new(InProcessCache::new(InProcessConfig::new(1024, 1024)));
    assert!(cache_from_parts(None, "site-1", Some(Handle::current())).is_none());
    assert!(cache_from_parts(Some(Arc::clone(&store)), "site-1", None).is_none());
    assert!(cache_from_parts(Some(store), "site-1", Some(Handle::current())).is_some());
}

// ---------------------------------------------------------------------------------------------
// retrieval index
// ---------------------------------------------------------------------------------------------

/// A lag the test steers: 0 caught up, 1 behind, anything else unknown.
struct Lag(AtomicU8);

impl IndexLag for Lag {
    fn behind(&self) -> LagFuture<'_> {
        let state = self.0.load(Ordering::SeqCst);
        Box::pin(async move {
            match state {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            }
        })
    }
}

/// A `JobSubmit` that records jobs and coalesces on the idempotency key while the job is "queued".
struct Submit {
    queued: Mutex<HashSet<String>>,
    jobs: Mutex<Vec<JobSpec>>,
}

impl Submit {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::new(HashSet::new()),
            jobs: Mutex::new(Vec::new()),
        })
    }

    fn count(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    /// The backfill finished: the key is free again.
    fn finish(&self) {
        self.queued.lock().unwrap().clear();
    }
}

impl JobSubmit for Submit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            let fresh = self
                .queued
                .lock()
                .unwrap()
                .insert(job.idempotency_key.clone());
            if fresh {
                self.jobs.lock().unwrap().push(job);
            }
            Ok(fresh)
        })
    }
}

struct IndexRig {
    lag: Arc<Lag>,
    submit: Arc<Submit>,
    clock: Arc<ManualClock>,
    part: Arc<RetrievalIndexPart>,
}

async fn index_rig(lag: u8) -> IndexRig {
    let lag = Arc::new(Lag(AtomicU8::new(lag)));
    let submit = Submit::new();
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ));
    let part = Arc::new(RetrievalIndexPart::new(
        Arc::clone(&lag) as Arc<dyn IndexLag>,
        Arc::clone(&submit) as Arc<dyn JobSubmit>,
        "default".to_owned(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Handle::current(),
    ));
    IndexRig {
        lag,
        submit,
        clock,
        part,
    }
}

fn retrieval() -> IndexRef {
    IndexRef::new(INDEX_NAME)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staleness_is_the_cursor_behind_the_feed_head() {
    let rig = index_rig(1).await;
    let part = Arc::clone(&rig.part);
    let (stale, view) = blocking(move || {
        let ops = part.live();
        (ops.stale(), ops.index(&retrieval()).unwrap())
    })
    .await;
    assert_eq!(stale, vec![retrieval()]);
    assert_eq!((view.health, view.generation), (IndexHealth::Stale, 0));

    // Caught up, or a feed that cannot be read: nothing stale is claimed.
    for state in [0, 2] {
        rig.lag.0.store(state, Ordering::SeqCst);
        let part = Arc::clone(&rig.part);
        let stale = blocking(move || part.live().stale()).await;
        assert!(stale.is_empty(), "lag state {state}");
    }
    let part = Arc::clone(&rig.part);
    let none = blocking(move || part.live().index(&IndexRef::new("other"))).await;
    assert!(none.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rebuild_bumps_the_generation_once_and_enqueues_exactly_one_backfill() {
    let rig = index_rig(1).await;
    let part = Arc::clone(&rig.part);
    let (change, after) = blocking(move || {
        let mut ops = part.live();
        let change = ops.rebuild(&retrieval()).unwrap();
        (change, ops.index(&retrieval()).unwrap())
    })
    .await;
    assert_eq!(
        (
            change.before,
            change.after,
            change.before_generation,
            change.after_generation
        ),
        (IndexHealth::Stale, IndexHealth::Fresh, 0, 1)
    );
    assert_eq!(rig.submit.count(), 1);
    let job = rig.submit.jobs.lock().unwrap()[0].clone();
    assert_eq!(job.name, BACKFILL_JOB);
    assert_eq!(job.queue, "default");
    // The backfill is in flight: a rebuild is not asked for again.
    assert_eq!((after.health, after.generation), (IndexHealth::Fresh, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_rebuild_while_one_is_queued_is_refused() {
    let rig = index_rig(1).await;
    let part = Arc::clone(&rig.part);
    let second = blocking(move || {
        let mut ops = part.live();
        ops.rebuild(&retrieval()).unwrap();
        ops.rebuild(&retrieval())
    })
    .await
    .unwrap_err();
    assert!(second.detail.contains("already queued"), "{second}");
    assert_eq!(rig.submit.count(), 1, "exactly one backfill job");
    let part = Arc::clone(&rig.part);
    let generation = blocking(move || part.live().index(&retrieval()).unwrap().generation).await;
    assert_eq!(generation, 1, "the refused rebuild did not bump it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backfill_that_never_ran_reads_stale_again_after_the_window() {
    let rig = index_rig(1).await;
    let part = Arc::clone(&rig.part);
    blocking(move || part.live().rebuild(&retrieval()).unwrap()).await;
    rig.clock
        .advance(Duration::from_secs(REQUEST_WINDOW_SECS as u64 + 1));
    let part = Arc::clone(&rig.part);
    let view = blocking(move || part.live().index(&retrieval()).unwrap()).await;
    assert_eq!(view.health, IndexHealth::Stale);
    assert_eq!(view.generation, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_puts_the_generation_back_only_if_nothing_newer_ran() {
    let rig = index_rig(1).await;
    let part = Arc::clone(&rig.part);
    let first: IndexChange = blocking(move || part.live().rebuild(&retrieval()).unwrap()).await;

    // Nothing newer ran: the previous generation is back.
    let part = Arc::clone(&rig.part);
    let undo = first.clone();
    let generation = blocking(move || {
        let mut ops = part.live();
        ops.restore(&undo).unwrap();
        ops.index(&retrieval()).unwrap().generation
    })
    .await;
    assert_eq!(generation, 0);

    // A rebuild, its backfill done, then another rebuild: the first change no longer restores.
    rig.submit.finish();
    let part = Arc::clone(&rig.part);
    let second = blocking(move || {
        let mut ops = part.live();
        let one = ops.rebuild(&retrieval()).unwrap();
        // The first backfill finished; the next rebuild is accepted.
        (one, ops.index(&retrieval()).unwrap().generation)
    })
    .await;
    assert_eq!(second.1, 1);
    rig.submit.finish();
    let part = Arc::clone(&rig.part);
    let stale_undo = second.0.clone();
    let error = blocking(move || {
        let mut ops = part.live();
        ops.rebuild(&retrieval()).unwrap();
        ops.restore(&stale_undo)
    })
    .await
    .unwrap_err();
    assert!(error.detail.contains("newer rebuild"), "{error}");
    assert_eq!(rig.submit.count(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_rebuild_never_enqueues_and_leaves_the_live_generation() {
    let rig = index_rig(1).await;
    let part = Arc::clone(&rig.part);
    let (change, live_generation) = blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        let change = ops.rebuild(&retrieval()).unwrap();
        (change, part.live().index(&retrieval()).unwrap().generation)
    })
    .await;
    assert_eq!(
        (change.before, change.after, change.after_generation),
        (IndexHealth::Stale, IndexHealth::Fresh, 1)
    );
    assert_eq!(live_generation, 0);
    assert_eq!(rig.submit.count(), 0, "a rehearsal never enqueues");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_part_missing_what_it_needs_is_none() {
    let rig = index_rig(1).await;
    let lag = Arc::clone(&rig.lag) as Arc<dyn IndexLag>;
    let submit = Arc::clone(&rig.submit) as Arc<dyn JobSubmit>;
    let clock = Arc::clone(&rig.clock) as Arc<dyn Clock>;
    let handle = Some(Handle::current());
    let queue = Some("default".to_owned());
    assert!(
        index_from_parts(
            None,
            Some(submit.clone()),
            queue.clone(),
            clock.clone(),
            handle.clone()
        )
        .is_none()
    );
    assert!(
        index_from_parts(
            Some(lag.clone()),
            None,
            queue.clone(),
            clock.clone(),
            handle.clone()
        )
        .is_none()
    );
    assert!(
        index_from_parts(
            Some(lag.clone()),
            Some(submit.clone()),
            None,
            clock.clone(),
            handle.clone()
        )
        .is_none()
    );
    assert!(
        index_from_parts(
            Some(lag.clone()),
            Some(submit.clone()),
            queue.clone(),
            clock.clone(),
            None
        )
        .is_none()
    );
    let part: Option<Arc<dyn IndexPart>> =
        index_from_parts(Some(lag), Some(submit), queue, clock, handle);
    assert!(part.is_some());
}
