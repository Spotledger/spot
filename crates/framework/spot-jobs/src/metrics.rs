//! Job queue metrics (WP-PLT-15.T5): how many jobs wait and how long the oldest has waited.
//!
//! The crate defines the names and the shape of what is published; a host turns the sink calls
//! into Prometheus series (`spot-cli`'s `services::telemetry::PrometheusSink`). The label is the
//! **queue** only, and only a fixed set of names: a queue this module does not know is counted
//! under `other`, so a typo or a hostile name can never create a series. No tenant, user or job
//! id ever appears (rule M3 of the metrics lint).

/// Gauge: jobs waiting to run (pending or due for retry), labelled `queue`.
pub const JOB_QUEUE_DEPTH: &str = "spot_job_queue_depth";
/// Gauge: seconds the oldest waiting job has waited, labelled `queue` (0 when none waits).
pub const JOB_OLDEST_AGE_SECONDS: &str = "spot_job_oldest_age_seconds";
/// Counter: jobs moved from dead or retrying back to pending by `JobStore::requeue`, labelled
/// `queue`. A refused requeue is not counted.
pub const JOB_REQUEUED_TOTAL: &str = "spot_job_requeued_total";

/// The queue labels, in the order they are published. `other` collects every other name.
pub const QUEUE_LABELS: [&str; 4] = ["default", "short", "long", "other"];

/// The bounded label for a queue name.
#[must_use]
pub fn queue_label(queue: &str) -> &'static str {
    QUEUE_LABELS
        .iter()
        .take(3)
        .find(|known| **known == queue)
        .copied()
        .unwrap_or("other")
}

/// What one queue looks like at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// The queue's name as the job store holds it.
    pub queue: String,
    /// Jobs waiting.
    pub depth: u64,
    /// Seconds the oldest waiting job has waited.
    pub oldest_age_seconds: u64,
}

/// Seconds a job enqueued at `enqueued_unix` has waited at `now_unix` (both Unix seconds). A job
/// from the future (clock skew) has waited zero.
#[must_use]
pub fn age_seconds(now_unix: i64, enqueued_unix: i64) -> u64 {
    u64::try_from(now_unix.saturating_sub(enqueued_unix)).unwrap_or(0)
}

/// Where the numbers go.
pub trait JobMetricsSink: Send + Sync {
    /// Sets the depth of the queue labelled `queue`.
    fn queue_depth(&self, queue: &'static str, depth: u64);
    /// Sets the age of the oldest waiting job of the queue labelled `queue`.
    fn oldest_age_seconds(&self, queue: &'static str, seconds: u64);
    /// Counts one requeued job of the queue labelled `queue` ([`JOB_REQUEUED_TOTAL`]). The
    /// default does nothing, so a sink written before requeue existed keeps compiling; a store
    /// built `with_metrics` calls it once per successful requeue.
    fn requeued(&self, _queue: &'static str) {}
}

/// Publishes `snapshots` to `sink`: every label in [`QUEUE_LABELS`] gets a value on every call
/// (a drained queue reads 0, never a stale number), rows that map to the same label are summed
/// (depth) or maxed (age).
pub fn publish(sink: &dyn JobMetricsSink, snapshots: &[QueueSnapshot]) {
    for label in QUEUE_LABELS {
        let mut depth = 0_u64;
        let mut age = 0_u64;
        for row in snapshots
            .iter()
            .filter(|row| queue_label(&row.queue) == label)
        {
            depth = depth.saturating_add(row.depth);
            age = age.max(row.oldest_age_seconds);
        }
        sink.queue_depth(label, depth);
        sink.oldest_age_seconds(label, age);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recording {
        depth: Mutex<BTreeMap<&'static str, u64>>,
        age: Mutex<BTreeMap<&'static str, u64>>,
    }

    impl JobMetricsSink for Recording {
        fn queue_depth(&self, queue: &'static str, depth: u64) {
            self.depth.lock().unwrap().insert(queue, depth);
        }

        fn oldest_age_seconds(&self, queue: &'static str, seconds: u64) {
            self.age.lock().unwrap().insert(queue, seconds);
        }
    }

    fn row(queue: &str, depth: u64, age: u64) -> QueueSnapshot {
        QueueSnapshot {
            queue: queue.to_owned(),
            depth,
            oldest_age_seconds: age,
        }
    }

    #[test]
    fn depth_and_age_move_with_the_snapshot() {
        let sink = Recording::default();
        publish(&sink, &[row("short", 3, 40), row("default", 10, 5)]);
        assert_eq!(sink.depth.lock().unwrap()["short"], 3);
        assert_eq!(sink.age.lock().unwrap()["short"], 40);
        publish(&sink, &[row("short", 1, 2)]);
        assert_eq!(sink.depth.lock().unwrap()["short"], 1);
        assert_eq!(
            sink.depth.lock().unwrap()["default"],
            0,
            "a drained queue reads zero"
        );
        assert_eq!(sink.age.lock().unwrap()["default"], 0);
    }

    #[test]
    fn labels_are_bounded_and_unknown_queues_share_other() {
        let sink = Recording::default();
        publish(
            &sink,
            &[
                row("tenant-42-secret", 2, 9),
                row("../x", 3, 30),
                row("long", 1, 1),
            ],
        );
        let depth = sink.depth.lock().unwrap();
        assert_eq!(depth.keys().copied().collect::<Vec<_>>(), {
            let mut all = QUEUE_LABELS.to_vec();
            all.sort_unstable();
            all
        });
        assert_eq!(depth["other"], 5);
        assert_eq!(
            sink.age.lock().unwrap()["other"],
            30,
            "the oldest of the merged rows"
        );
        assert_eq!(queue_label("tenant-42-secret"), "other");
        assert_eq!(queue_label("short"), "short");
    }

    #[test]
    fn a_sink_without_requeued_ignores_it_and_one_with_it_counts() {
        // The default body: a sink that never heard of requeue is a no-op, not a failure.
        let plain = Recording::default();
        plain.requeued("default");

        #[derive(Default)]
        struct Counting(Mutex<BTreeMap<&'static str, u64>>);
        impl JobMetricsSink for Counting {
            fn queue_depth(&self, _queue: &'static str, _depth: u64) {}
            fn oldest_age_seconds(&self, _queue: &'static str, _seconds: u64) {}
            fn requeued(&self, queue: &'static str) {
                *self.0.lock().unwrap().entry(queue).or_insert(0) += 1;
            }
        }
        let sink = Counting::default();
        sink.requeued(queue_label("short"));
        sink.requeued(queue_label("tenant-42-secret"));
        sink.requeued(queue_label("tenant-43-secret"));
        let counts = sink.0.lock().unwrap();
        assert_eq!(counts["short"], 1);
        assert_eq!(counts["other"], 2, "unknown queues share one label");
    }

    #[test]
    fn age_never_goes_negative() {
        assert_eq!(age_seconds(100, 40), 60);
        assert_eq!(age_seconds(40, 100), 0);
        assert_eq!(age_seconds(i64::MIN, i64::MAX), 0);
    }
}
