//! What an operator watches on the event stream: gauges from a sampler, counters at the port,
//! and the live readiness reason.
//!
//! The [`Sampler`] runs inside the service's loop every `metrics_interval_seconds`. It reads the
//! consumers (`StreamEngine::list` as the system), refreshes the outbox head the `tail` start
//! already uses, asks for the partitions parked past the report time, and publishes:
//!
//! | gauge | meaning |
//! | --- | --- |
//! | `spot_events_consumers` | registered consumers |
//! | `spot_events_open_streams` | open `event_stream` connections |
//! | `spot_events_parked_partitions` | partitions parked right now |
//! | `spot_events_oldest_parked_seconds` | age of the oldest parked partition (0 when none) |
//! | `spot_events_max_lag_events` | largest `head.seq - checkpoint.seq` of a consumer partition in the head's generation |
//! | `spot_events_resync_required` | consumers whose checkpoint is in an older generation than the head (a restore happened) |
//! | `spot_events_head_age_seconds` | time since the sampler last saw the head move |
//!
//! Positions of different generations are not comparable, so a consumer behind a generation
//! change contributes to `resync_required` and never to the lag. A consumer that has never
//! acknowledged anything (checkpoint at the feed's start) is behind by the head's sequence.
//!
//! The counters (`spot_events_pulled_total`, `_acked_total`, `_redelivered_total`,
//! `_skipped_total`, `_stream_rejections_total{reason}`, `_stream_closes_total{reason}`) are
//! counted where the port hands a result back. No label carries a consumer, user, topic or
//! position: a consumer name is administrator-chosen free text and unbounded. The three most
//! behind consumers' names (cut to 40 characters, control characters dropped) appear in the
//! readiness reason only.
//!
//! The readiness reason is computed by the sampler and read from memory by `Service::degraded`:
//! it never touches the database on a readiness call.

use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use spot_event_stream::ParkedConsumer;
use time::OffsetDateTime;

use super::*;

/// Gauge: registered consumers.
pub(super) const CONSUMERS: &str = "spot_events_consumers";
/// Gauge: open streams.
pub(super) const OPEN_STREAMS: &str = "spot_events_open_streams";
/// Gauge: parked partitions.
pub(super) const PARKED_PARTITIONS: &str = "spot_events_parked_partitions";
/// Gauge: age of the oldest parked partition, seconds.
pub(super) const OLDEST_PARKED_SECONDS: &str = "spot_events_oldest_parked_seconds";
/// Gauge: the largest consumer lag, events.
pub(super) const MAX_LAG_EVENTS: &str = "spot_events_max_lag_events";
/// Gauge: consumers whose checkpoint is in an older generation.
pub(super) const RESYNC_REQUIRED: &str = "spot_events_resync_required";
/// Gauge: seconds since the head last moved.
pub(super) const HEAD_AGE_SECONDS: &str = "spot_events_head_age_seconds";
/// Counter: events handed out by pulls.
pub(super) const PULLED_TOTAL: &str = "spot_events_pulled_total";
/// Counter: acknowledgements accepted.
pub(super) const ACKED_TOTAL: &str = "spot_events_acked_total";
/// Counter: events delivered again before their acknowledgement.
pub(super) const REDELIVERED_TOTAL: &str = "spot_events_redelivered_total";
/// Counter: events the owner may not read, left out.
pub(super) const SKIPPED_TOTAL: &str = "spot_events_skipped_total";
/// Counter: streams refused by a connection budget, by `reason`.
pub(super) const STREAM_REJECTIONS_TOTAL: &str = "spot_events_stream_rejections_total";
/// Counter: streams that ended, by `reason`.
pub(super) const STREAM_CLOSES_TOTAL: &str = "spot_events_stream_closes_total";

/// Consecutive sampling failures before readiness says so.
const FAILURES_BEFORE_DEGRADED: u32 = 3;

/// The longest consumer name the readiness reason shows.
const NAME_SHOWN: usize = 40;

/// How many lagging consumers the readiness reason names.
const NAMES_SHOWN: usize = 3;

/// What one sample found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Sample {
    pub(super) consumers: usize,
    /// Partitions parked right now.
    pub(super) parked: usize,
    pub(super) oldest_parked_seconds: u64,
    pub(super) max_lag: u64,
    pub(super) resync_required: usize,
    /// The most behind consumers, most behind first, with their lag.
    pub(super) top_lag: Vec<(String, u64)>,
    /// Partitions parked at least the report time ago.
    pub(super) reported: usize,
    pub(super) oldest_reported_seconds: u64,
}

fn seconds_between(now: OffsetDateTime, since: OffsetDateTime) -> u64 {
    u64::try_from((now - since).whole_seconds()).unwrap_or(0)
}

/// Works the sample out of the consumers, the head and the parked report. Pure: no clock, no I/O.
pub(super) fn compute(
    records: &[ConsumerRecord],
    head: Option<Position>,
    reported: &[ParkedConsumer],
    now: OffsetDateTime,
) -> Sample {
    let mut sample = Sample {
        consumers: records.len(),
        ..Sample::default()
    };
    let mut lagging: Vec<(String, u64)> = Vec::new();
    for record in records {
        let mut lag = 0_u64;
        let mut behind_a_generation = false;
        for (index, checkpoint) in record.checkpoints.iter().enumerate() {
            if record.parked.get(index).copied().unwrap_or(false) {
                sample.parked += 1;
                if let Some(Some(since)) = record.parked_since.get(index) {
                    sample.oldest_parked_seconds = sample
                        .oldest_parked_seconds
                        .max(seconds_between(now, *since));
                }
            }
            let Some(head) = head else { continue };
            let at_start = checkpoint.generation == 0 && checkpoint.seq == 0;
            if checkpoint.generation == head.generation || at_start {
                // A checkpoint at the feed's start has seen nothing: behind by the whole
                // generation the head is in.
                let seen = if at_start && head.generation != 0 {
                    0
                } else {
                    checkpoint.seq
                };
                lag = lag.max(head.seq.saturating_sub(seen));
            } else if checkpoint.generation < head.generation {
                behind_a_generation = true;
            }
            // A checkpoint ahead of a stale head cache is not behind anything.
        }
        if behind_a_generation {
            sample.resync_required += 1;
        }
        if lag > 0 {
            lagging.push((record.spec.name.clone(), lag));
        }
        sample.max_lag = sample.max_lag.max(lag);
    }
    lagging.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    lagging.truncate(NAMES_SHOWN);
    sample.top_lag = lagging;
    sample.reported = reported.len();
    sample.oldest_reported_seconds = reported
        .iter()
        .map(|p| seconds_between(now, p.since))
        .max()
        .unwrap_or(0);
    sample
}

/// A consumer name as the readiness reason shows it: printable, at most [`NAME_SHOWN`] characters.
fn shown(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(NAME_SHOWN)
        .collect()
}

/// Why readiness is degraded for `sample`, or `None`. Names no position and no subject.
pub(super) fn reason(sample: &Sample, lag_warn_events: u64) -> Option<String> {
    let mut parts = Vec::new();
    if sample.reported > 0 {
        parts.push(format!(
            "{} partition(s) parked, the oldest for {} s",
            sample.reported, sample.oldest_reported_seconds
        ));
    }
    if lag_warn_events > 0 && sample.max_lag > lag_warn_events {
        let names: Vec<String> = sample.top_lag.iter().map(|(n, _)| shown(n)).collect();
        parts.push(format!(
            "consumer lag {} events (limit {lag_warn_events}): {}",
            sample.max_lag,
            names.join(", ")
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

#[derive(Default)]
struct Watched {
    /// The head the last sample saw, and when it last differed from the one before.
    head: Option<FeedPosition>,
    head_moved: Option<Duration>,
    /// The parked partitions the last warning named.
    parked: Vec<(String, u32)>,
}

/// Samples the stream on an interval and keeps the readiness reason.
#[derive(Default)]
pub(super) struct Sampler {
    watched: Mutex<Watched>,
    reason: Mutex<Option<String>>,
    failures: AtomicU32,
}

#[allow(clippy::cast_precision_loss)]
fn gauge(name: &'static str, value: u64) {
    metrics::gauge!(name).set(value as f64);
}

impl Sampler {
    /// The live readiness reason from the last sample (an in-memory read).
    pub(super) fn reason(&self) -> Option<String> {
        lock(&self.reason).clone()
    }

    fn set_reason(&self, reason: Option<String>) {
        *lock(&self.reason) = reason;
    }

    /// One sample: reads, publishes the gauges and refreshes the readiness reason. A failed read
    /// is counted; the third in a row degrades readiness until a sample succeeds.
    pub(super) async fn sample(&self, shared: &Shared) {
        match self.read(shared).await {
            Ok(sample) => {
                self.failures.store(0, AtomicOrdering::Relaxed);
                self.publish(shared, &sample);
                self.set_reason(reason(&sample, shared.settings.lag_warn_events));
            }
            Err(code) => {
                let failed = self.failures.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                tracing::warn!(site = %shared.site, code = %code, failed, "the event stream could not be sampled");
                if failed >= FAILURES_BEFORE_DEGRADED {
                    self.set_reason(Some("cannot sample".to_owned()));
                }
            }
        }
    }

    async fn read(&self, shared: &Shared) -> std::result::Result<Sample, String> {
        let records = shared
            .engine
            .list(&Principal::system())
            .await
            .map_err(|e| e.code().to_owned())?;
        // The cache is refreshed from the last known head: a cheap read of what is new.
        shared
            .head
            .refresh(&shared.backend)
            .await
            .map_err(|e| e.code.as_str().to_owned())?;
        let reported = shared
            .engine
            .parked_report()
            .await
            .map_err(|e| e.code().to_owned())?;
        let head = shared
            .head
            .head()
            .map(spot_event_stream::position_from_feed);
        let now = shared.clock.now_utc();
        let sample = compute(&records, head, &reported, now);
        self.note_head(shared);
        self.note_parked(shared, &reported);
        Ok(sample)
    }

    /// Remembers when the head last moved.
    fn note_head(&self, shared: &Shared) {
        let now = shared.clock.monotonic();
        let head = shared.head.head();
        let mut watched = lock(&self.watched);
        if watched.head != head || watched.head_moved.is_none() {
            watched.head = head;
            watched.head_moved = Some(now);
        }
    }

    /// Warns once when the set of parked partitions changed, never per sample.
    fn note_parked(&self, shared: &Shared, reported: &[ParkedConsumer]) {
        let mut now: Vec<(String, u32)> = reported
            .iter()
            .map(|p| (p.consumer.clone(), p.partition))
            .collect();
        now.sort();
        let mut watched = lock(&self.watched);
        if watched.parked != now {
            if !now.is_empty() {
                tracing::warn!(
                    site = %shared.site,
                    parked = now.len(),
                    "event consumers are parked: an operator restarts the partition"
                );
            }
            watched.parked = now;
        }
    }

    fn publish(&self, shared: &Shared, sample: &Sample) {
        gauge(CONSUMERS, sample.consumers as u64);
        gauge(OPEN_STREAMS, lock(&shared.streams).total as u64);
        gauge(PARKED_PARTITIONS, sample.parked as u64);
        gauge(OLDEST_PARKED_SECONDS, sample.oldest_parked_seconds);
        gauge(MAX_LAG_EVENTS, sample.max_lag);
        gauge(RESYNC_REQUIRED, sample.resync_required as u64);
        let age = {
            let watched = lock(&self.watched);
            watched.head_moved.map_or(0, |at| {
                shared.clock.monotonic().saturating_sub(at).as_secs()
            })
        };
        gauge(HEAD_AGE_SECONDS, age);
    }
}

// ---------------------------------------------------------------------------------------------
// Counters at the port
// ---------------------------------------------------------------------------------------------

/// The highest position this process delivered on `(consumer, partition)` when a batch is still
/// waiting for its acknowledgement: events at or before it that a pull returns again are
/// redeliveries. `None` when nothing is outstanding.
pub(super) fn redeliverable(shared: &Shared, consumer: &str, partition: u32) -> Option<Position> {
    let key = (consumer.to_owned(), partition);
    if !lock(&shared.outstanding).contains(&key) {
        return None;
    }
    lock(&shared.sent).get(&key).copied()
}

/// Counts what a pull handed out: events, the ones left out for the owner's rights, and the ones
/// that were delivered before and not yet acknowledged.
pub(super) fn pulled(out: &PullOut, redeliverable: Option<Position>) {
    let events = out.events.len() as u64;
    if events > 0 {
        metrics::counter!(PULLED_TOTAL).increment(events);
    }
    if out.skipped > 0 {
        metrics::counter!(SKIPPED_TOTAL).increment(out.skipped);
    }
    let Some(top) = redeliverable else { return };
    let again = out
        .events
        .iter()
        .filter_map(|event| event.get("position"))
        .filter_map(|p| serde_json::from_value::<Position>(p.clone()).ok())
        .filter(|p| order(*p) <= order(top))
        .count() as u64;
    if again > 0 {
        metrics::counter!(REDELIVERED_TOTAL).increment(again);
    }
}

/// Counts one accepted acknowledgement.
pub(super) fn acked() {
    metrics::counter!(ACKED_TOTAL).increment(1);
}

/// Counts a stream refused by a connection budget (`site_cap` when the site's budget is full,
/// else `user_cap`); any other failure is not a refusal of the budget.
pub(super) fn refused(shared: &Shared, error: &ApiError) {
    if error.code.as_str() != "api.rate_limited" {
        return;
    }
    let site_full = lock(&shared.streams).total >= shared.settings.max_streams;
    let reason = if site_full { "site_cap" } else { "user_cap" };
    metrics::counter!(STREAM_REJECTIONS_TOTAL, "reason" => reason).increment(1);
}

/// An open stream that counts how it ends: `shutdown` (the stop signal), `error` (the service
/// ended it: a lost lease, revoked rights, a resync) or `client` (the route dropped it: the
/// client left, or the idle limit ended it).
struct CountedFeed {
    inner: Box<dyn EventFeed>,
    stop: watch::Receiver<bool>,
    counted: bool,
}

impl EventFeed for CountedFeed {
    fn lease(&self) -> &str {
        self.inner.lease()
    }

    fn next(&mut self) -> BoxFuture<'_, FeedItem> {
        Box::pin(async move {
            let item = self.inner.next().await;
            if matches!(item, FeedItem::Closed) && !self.counted {
                self.counted = true;
                let reason = if *self.stop.borrow() {
                    "shutdown"
                } else {
                    "error"
                };
                metrics::counter!(STREAM_CLOSES_TOTAL, "reason" => reason).increment(1);
            }
            item
        })
    }
}

impl Drop for CountedFeed {
    fn drop(&mut self) {
        if !self.counted {
            metrics::counter!(STREAM_CLOSES_TOTAL, "reason" => "client").increment(1);
        }
    }
}

/// Wraps an opened stream so its end is counted.
pub(super) fn counted(shared: &Shared, feed: Box<dyn EventFeed>) -> Box<dyn EventFeed> {
    Box::new(CountedFeed {
        inner: feed,
        stop: shared.stop.subscribe(),
        counted: false,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_event_stream::ConsumerSpec;
    use time::macros::datetime;

    fn record(
        name: &str,
        checkpoints: &[(u64, u64)],
        parked: &[Option<OffsetDateTime>],
    ) -> ConsumerRecord {
        let spec = ConsumerSpec {
            name: name.to_owned(),
            owner: "u".to_owned(),
            topics: Vec::new(),
            filter: None,
            partitions: u32::try_from(checkpoints.len()).unwrap(),
            payload_mode: PayloadMode::IdsOnly,
            start: Start::Tail,
            classes: None,
            entities: None,
        };
        ConsumerRecord {
            spec,
            checkpoints: checkpoints
                .iter()
                .map(|(generation, seq)| Position {
                    generation: *generation,
                    seq: *seq,
                })
                .collect(),
            parked: parked.iter().map(Option::is_some).collect(),
            parked_since: parked.to_vec(),
            registered_at: datetime!(2026-11-13 00:00 UTC),
        }
    }

    const NOW: OffsetDateTime = datetime!(2026-11-13 12:00 UTC);

    #[test]
    fn lag_is_head_minus_checkpoint_in_the_same_generation() {
        let head = Some(Position {
            generation: 2,
            seq: 400,
        });
        let records = [
            record("a", &[(2, 150)], &[None]),
            record("b", &[(2, 400)], &[None]),
        ];
        let sample = compute(&records, head, &[], NOW);
        assert_eq!(sample.max_lag, 250);
        assert_eq!(sample.top_lag, vec![("a".to_owned(), 250)]);
        assert_eq!(sample.consumers, 2);
        assert_eq!(sample.resync_required, 0);
    }

    #[test]
    fn an_older_generation_is_a_resync_not_a_lag() {
        let head = Some(Position {
            generation: 3,
            seq: 10,
        });
        let records = [record("old", &[(2, 9_999_999)], &[None])];
        let sample = compute(&records, head, &[], NOW);
        assert_eq!(sample.resync_required, 1);
        assert_eq!(sample.max_lag, 0);
    }

    #[test]
    fn a_checkpoint_ahead_of_a_stale_head_is_not_a_negative_lag() {
        let head = Some(Position {
            generation: 1,
            seq: 5,
        });
        let records = [
            record("ahead", &[(1, 50)], &[None]),
            record("newer", &[(2, 1)], &[None]),
        ];
        let sample = compute(&records, head, &[], NOW);
        assert_eq!(sample.max_lag, 0);
        assert_eq!(sample.resync_required, 0);
    }

    #[test]
    fn a_consumer_that_never_acknowledged_is_behind_by_the_head_sequence() {
        let head = Some(Position {
            generation: 2,
            seq: 40,
        });
        let records = [record("fresh", &[(0, 0)], &[None])];
        assert_eq!(compute(&records, head, &[], NOW).max_lag, 40);
    }

    #[test]
    fn without_a_head_there_is_no_lag() {
        let records = [record("a", &[(1, 1)], &[None])];
        assert_eq!(compute(&records, None, &[], NOW).max_lag, 0);
    }

    #[test]
    fn parked_partitions_are_counted_and_aged() {
        let since = datetime!(2026-11-13 11:00 UTC);
        let records = [record("p", &[(1, 1), (1, 1)], &[Some(since), None])];
        let reported = [ParkedConsumer {
            consumer: "p".to_owned(),
            partition: 0,
            since,
        }];
        let sample = compute(&records, None, &reported, NOW);
        assert_eq!(sample.parked, 1);
        assert_eq!(sample.oldest_parked_seconds, 3600);
        assert_eq!(sample.reported, 1);
        assert_eq!(sample.oldest_reported_seconds, 3600);
    }

    #[test]
    fn the_reason_names_counts_and_at_most_three_truncated_names() {
        let sample = Sample {
            max_lag: 150,
            top_lag: vec![
                ("x".repeat(80), 150),
                ("b\nc".to_owned(), 100),
                ("c".to_owned(), 90),
            ],
            reported: 2,
            oldest_reported_seconds: 1200,
            ..Sample::default()
        };
        let text = reason(&sample, 100).unwrap();
        assert!(
            text.contains("2 partition(s) parked, the oldest for 1200 s"),
            "{text}"
        );
        assert!(text.contains("150 events (limit 100)"), "{text}");
        assert!(
            text.contains(&"x".repeat(40)) && !text.contains(&"x".repeat(41)),
            "{text}"
        );
        assert!(text.contains("bc") && !text.contains('\n'), "{text}");
        assert_eq!(
            reason(&sample, 0).unwrap(),
            "2 partition(s) parked, the oldest for 1200 s"
        );
        assert_eq!(reason(&Sample::default(), 100), None);
        let calm = Sample {
            max_lag: 100,
            ..Sample::default()
        };
        assert_eq!(reason(&calm, 100), None, "at the limit is not over it");
    }
}
