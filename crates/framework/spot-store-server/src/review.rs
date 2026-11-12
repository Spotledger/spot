//! The review workers.
//!
//! `spot_app_review::review` is CPU bound and synchronous. It runs on the server's own OS
//! threads, never on the HTTP runtime, with no network (the no-network [`NullProbe`]) and a
//! wall-clock limit. The review is cooperative, so a review that overruns cannot be stopped:
//! it is recorded as `failed` with the timeout code, and its thread keeps its permit until the
//! review returns. When every permit is held by such a stuck review, [`Reviewer::accepting`]
//! turns false and a publish is refused with `503` instead of piling work up.
//!
//! The dispatcher threads only wait, call the state store and hand the bytes to a runner thread,
//! so the store's synchronous calls always run on plain OS threads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use spot_app_review::{NullProbe, Report, ReviewConfig, Verdict, review};
use spot_registry::{BlobStore, Digest, collect_stream};
use spot_store_state::{ReviewStatus, ReviewSummary, ReviewUpdate, StoreStateStore};

use crate::clock::Clock;
use crate::error::codes;
use crate::exec::block_on;
use crate::metrics::Metrics;

/// One review to run: the stored bundle is read by digest in the worker, so a waiting job holds
/// no bundle bytes.
pub(crate) struct ReviewJob {
    pub review_id: String,
    pub digest: Digest,
}

/// Counting permits: at most `workers` reviews run at once, stuck ones included.
struct Permits {
    free: Mutex<usize>,
    wake: Condvar,
}

impl Permits {
    fn acquire(&self) {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        while *free == 0 {
            free = self.wake.wait(free).unwrap_or_else(PoisonError::into_inner);
        }
        *free -= 1;
    }

    fn release(&self) {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        *free += 1;
        self.wake.notify_one();
    }
}

/// What a runner and its dispatcher agree on, so a late finish is counted exactly once.
#[derive(Default)]
struct RunState {
    done: bool,
    timed_out: bool,
}

/// Held by the runner thread; releases the permit when the review returns (or panics).
struct RunnerGuard {
    permits: Arc<Permits>,
    state: Arc<Mutex<RunState>>,
    stuck: Arc<AtomicUsize>,
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.done = true;
            if state.timed_out {
                self.stuck.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.permits.release();
    }
}

struct Context {
    state: Arc<dyn StoreStateStore>,
    blobs: Arc<dyn BlobStore>,
    clock: Arc<dyn Clock>,
    config: Arc<ReviewConfig>,
    metrics: Arc<Metrics>,
    timeout: Duration,
    max_bundle: u64,
    permits: Arc<Permits>,
    stuck: Arc<AtomicUsize>,
}

/// The review queue and its workers.
pub(crate) struct Reviewer {
    sender: SyncSender<ReviewJob>,
    stuck: Arc<AtomicUsize>,
    workers: usize,
}

/// What [`Reviewer::start`] needs from the service.
pub(crate) struct ReviewDeps {
    pub state: Arc<dyn StoreStateStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub clock: Arc<dyn Clock>,
    pub metrics: Arc<Metrics>,
    pub config: ReviewConfig,
    pub workers: usize,
    pub queue: usize,
    pub timeout: Duration,
    pub max_bundle: u64,
}

impl Reviewer {
    /// Starts the dispatcher threads.
    pub fn start(deps: ReviewDeps) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel::<ReviewJob>(deps.queue);
        let receiver = Arc::new(Mutex::new(receiver));
        let stuck = Arc::new(AtomicUsize::new(0));
        let context = Arc::new(Context {
            state: deps.state,
            blobs: deps.blobs,
            clock: deps.clock,
            config: Arc::new(deps.config),
            metrics: deps.metrics,
            timeout: deps.timeout,
            max_bundle: deps.max_bundle,
            permits: Arc::new(Permits {
                free: Mutex::new(deps.workers),
                wake: Condvar::new(),
            }),
            stuck: Arc::clone(&stuck),
        });
        for index in 0..deps.workers {
            let receiver = Arc::clone(&receiver);
            let context = Arc::clone(&context);
            std::thread::Builder::new()
                .name(format!("store-review-{index}"))
                .spawn(move || dispatch(&receiver, &context))?;
        }
        Ok(Self {
            sender,
            stuck,
            workers: deps.workers,
        })
    }

    /// Whether a new publish may start: at least one review worker is not stuck.
    pub fn accepting(&self) -> bool {
        self.stuck.load(Ordering::SeqCst) < self.workers
    }

    /// How many reviews are past their limit and still running (a gauge for `/metrics`).
    pub fn stuck(&self) -> usize {
        self.stuck.load(Ordering::SeqCst)
    }

    /// Queues a review without waiting; `Err` when the queue is full or the workers are gone.
    pub fn enqueue(&self, job: ReviewJob) -> Result<(), ()> {
        match self.sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(()),
        }
    }
}

fn dispatch(receiver: &Mutex<Receiver<ReviewJob>>, context: &Context) {
    loop {
        let job = {
            let guard = receiver.lock().unwrap_or_else(PoisonError::into_inner);
            match guard.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        context.run(&job);
    }
}

/// The wire word of a verdict.
fn verdict_word(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Rejected => "rejected",
        Verdict::NeedsHumanReview => "needs_human_review",
        Verdict::LooksClean => "looks_clean",
    }
}

/// The summary a publisher may read: the verdict and counts by severity, never the findings.
pub(crate) fn summary_json(report: &Report) -> String {
    let counts = &report.counts;
    let warning = counts
        .low
        .saturating_add(counts.medium)
        .saturating_add(counts.high);
    serde_json::json!({
        "verdict": verdict_word(report.verdict),
        "findings": {
            "info": counts.info,
            "warning": warning,
            "blocker": counts.blocker,
        },
    })
    .to_string()
}

impl Context {
    fn run(&self, job: &ReviewJob) {
        let now = self.clock.now();
        let running = ReviewUpdate {
            status: ReviewStatus::Running,
            verdict: None,
            summary: None,
            error_code: None,
            now,
        };
        if self.state.update_review(&job.review_id, &running).is_err() {
            // The review record cannot be updated: the state store is down, nothing to report to.
            Metrics::bump(&self.metrics.reviews_failed);
            return;
        }
        let bytes = match block_on(self.read_bundle(job.digest)) {
            Some(bytes) => bytes,
            None => {
                self.fail(&job.review_id, codes::REVIEW_FAILED);
                return;
            }
        };
        self.permits.acquire();
        let run_state = Arc::new(Mutex::new(RunState::default()));
        let guard = RunnerGuard {
            permits: Arc::clone(&self.permits),
            state: Arc::clone(&run_state),
            stuck: Arc::clone(&self.stuck),
        };
        let (reply, result) = channel::<Report>();
        let config = Arc::clone(&self.config);
        let spawned = std::thread::Builder::new()
            .name("store-review-run".to_owned())
            .spawn(move || {
                let _guard = guard;
                let report = review(&bytes, &config, &NullProbe);
                let _ = reply.send(report);
            });
        if spawned.is_err() {
            // The closure (and with it the guard) was dropped by the failed spawn, which
            // released the permit.
            self.fail(&job.review_id, codes::REVIEW_FAILED);
            return;
        }
        match result.recv_timeout(self.timeout) {
            Ok(report) => self.finish(&job.review_id, &report),
            Err(RecvTimeoutError::Timeout) => {
                let mut state = run_state.lock().unwrap_or_else(PoisonError::into_inner);
                if state.done {
                    drop(state);
                    match result.try_recv() {
                        Ok(report) => self.finish(&job.review_id, &report),
                        Err(_) => self.fail(&job.review_id, codes::REVIEW_FAILED),
                    }
                } else {
                    state.timed_out = true;
                    self.stuck.fetch_add(1, Ordering::SeqCst);
                    drop(state);
                    self.fail(&job.review_id, codes::REVIEW_TIMEOUT);
                }
            }
            Err(RecvTimeoutError::Disconnected) => self.fail(&job.review_id, codes::REVIEW_FAILED),
        }
    }

    async fn read_bundle(&self, digest: Digest) -> Option<Vec<u8>> {
        let stream = self.blobs.get(digest).await.ok()?;
        collect_stream(stream, self.max_bundle).await.ok()
    }

    fn finish(&self, review_id: &str, report: &Report) {
        let summary = match ReviewSummary::new(summary_json(report)) {
            Ok(summary) => summary,
            Err(_) => {
                self.fail(review_id, codes::REVIEW_FAILED);
                return;
            }
        };
        let update = ReviewUpdate {
            status: ReviewStatus::Done,
            verdict: Some(verdict_word(report.verdict).to_owned()),
            summary: Some(summary),
            error_code: None,
            now: self.clock.now(),
        };
        if self.state.update_review(review_id, &update).is_ok() {
            Metrics::bump(&self.metrics.reviews_finished);
        } else {
            Metrics::bump(&self.metrics.reviews_failed);
        }
    }

    fn fail(&self, review_id: &str, code: &'static str) {
        let update = ReviewUpdate {
            status: ReviewStatus::Failed,
            verdict: None,
            summary: None,
            error_code: Some(code.to_owned()),
            now: self.clock.now(),
        };
        let _ = self.state.update_review(review_id, &update);
        Metrics::bump(&self.metrics.reviews_failed);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Permits, summary_json};
    use spot_app_review::{NullProbe, ReviewConfig, review};
    use std::sync::{Arc, Condvar, Mutex};

    #[test]
    fn a_report_on_garbage_is_rejected_and_the_summary_has_counts_only() {
        let report = review(b"not a zip", &ReviewConfig::default(), &NullProbe);
        let text = summary_json(&report);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["verdict"], "rejected");
        assert!(value["findings"]["blocker"].as_u64().unwrap() >= 1);
        // The summary is counts and a verdict, never finding text.
        assert!(value.as_object().unwrap().len() == 2);
        assert!(!text.contains("message"));
    }

    #[test]
    fn permits_block_at_zero_and_wake_on_release() {
        let permits = Arc::new(Permits {
            free: Mutex::new(1),
            wake: Condvar::new(),
        });
        permits.acquire();
        let other = Arc::clone(&permits);
        let waiter = std::thread::spawn(move || {
            other.acquire();
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished());
        permits.release();
        waiter.join().unwrap();
    }
}
