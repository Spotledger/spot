//! One pass: read both streams from the watermark, compare them, and decide what is settled.
//!
//! # What a pass proves, and when
//!
//! The change feed and the journal's events are published by two statements, so the two sides of
//! one write reach their positions at slightly different instants. A pass therefore never judges
//! an entry younger than the **lag**: a finding is *settled* only when every position it names
//! was published at least `lag` before the snapshot (see `Heads::feed_stable`). A younger finding
//! is *deferred* to a later pass. The lag must exceed the longest gap between the two
//! publications; it is an argument, default [`DEFAULT_LAG_SECS`].
//!
//! # Resuming
//!
//! A pass starts `lag` before the older watermark entry in both streams (so a document whose two
//! sides straddle the watermark is compared whole), reads to the head, and only reports findings
//! that name a position after the watermark. The new watermark is the stable head of each stream,
//! **held back below the earliest open finding** of that stream: a bypassed write keeps the
//! watermark behind it, so the finding is found again by every pass and clears only when a later
//! pass finds the entry matched. Restarting the host therefore loses nothing.
//!
//! # Memory
//!
//! Pages are `page_size` entries and the checker tracks at most `window` versions; nothing else
//! grows with the tables. An entry that leaves the window unmatched is an `OutOfWindow` finding,
//! which is only raised when the window is larger than the measured lag (the entries published
//! in the last `lag` seconds): a smaller window may simply be too small, and says so in the
//! warning, not in a red condition.

use deadpool_postgres::Pool;
use spot_capture_invariant::{Checker, Exclusions, Finding, FindingKind, Report};

use crate::error::CapturePgError;
use crate::snapshot::Snapshot;
use crate::watermark::Watermarks;

/// The default window: tracked document versions.
pub const DEFAULT_WINDOW: usize = 50_000;
/// The default page size: entries read per statement.
pub const DEFAULT_PAGE_SIZE: usize = 2_000;
/// The default lag: seconds a position must have been published before it is judged.
pub const DEFAULT_LAG_SECS: u64 = 30;

/// How a pass reads and judges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanConfig {
    /// Entries per page; at least 1.
    pub page_size: usize,
    /// The checker's window; at least 1.
    pub window: usize,
    /// Seconds a position must have been published before a finding about it is settled.
    pub lag_secs: u64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            window: DEFAULT_WINDOW,
            lag_secs: DEFAULT_LAG_SECS,
        }
    }
}

/// The result of one pass.
#[derive(Clone, Debug)]
pub struct Scan {
    /// The checker's whole report (every finding, settled or not).
    pub report: Report,
    /// Settled findings of the kinds that raise `CAPTURE_MISMATCH`: a missing event, an orphan
    /// event or a version mismatch, and an out-of-window entry when the window exceeds the lag.
    pub raise: Vec<Finding>,
    /// Settled findings that do not raise: duplicate events and out-of-window entries of a
    /// window not larger than the lag. Counted and logged by the host.
    pub noted: Vec<Finding>,
    /// Findings about entries younger than the lag, judged by a later pass.
    pub deferred: usize,
    /// Derived exclusions the engine declares without a projector name (not a record finding).
    pub derived_without_projector: usize,
    /// The measured lag: entries published in the last `lag` seconds, the larger of the streams.
    pub lag_entries: u64,
    /// Whether the window is larger than [`Scan::lag_entries`].
    pub window_exceeds_lag: bool,
    /// Where the pass started reading from (the watermarks it resumed).
    pub from: Watermarks,
    /// The watermarks to store.
    pub next: Watermarks,
}

/// Runs one pass over `pool`'s database.
///
/// # Errors
///
/// [`CapturePgError::OptionsInvalid`] for a zero page size or window, the readers' errors, and
/// [`CapturePgError::CheckerRefused`] when a stream's positions do not increase.
pub async fn scan(
    pool: &Pool,
    exclusions: Exclusions,
    from: Watermarks,
    config: &ScanConfig,
) -> Result<Scan, CapturePgError> {
    if config.page_size == 0 || config.window == 0 {
        return Err(CapturePgError::OptionsInvalid);
    }
    let snapshot = Snapshot::open(pool).await?;
    let outcome = scan_in(&snapshot, exclusions, from, config).await;
    snapshot.finish().await;
    outcome
}

async fn scan_in(
    snapshot: &Snapshot,
    exclusions: Exclusions,
    from: Watermarks,
    config: &ScanConfig,
) -> Result<Scan, CapturePgError> {
    let heads = snapshot.heads(config.lag_secs).await?;
    // A new generation starts again at 0: what was settled before it is not a position of it.
    let from = if from.generation == heads.generation {
        from
    } else {
        Watermarks {
            generation: heads.generation,
            feed: 0,
            events: 0,
        }
    };
    let (mut feed_at, mut events_at) = snapshot
        .starts(heads.generation, from.feed, from.events, config.lag_secs)
        .await?;
    let mut checker =
        Checker::new(exclusions, config.window).map_err(|_| CapturePgError::OptionsInvalid)?;
    let (feed, events) = (snapshot.feed(), snapshot.events());
    let (mut feed_done, mut events_done) = (false, false);
    let mut feed_after_stable: u64 = 0;
    let mut events_after_stable: u64 = 0;
    while !(feed_done && events_done) {
        if !feed_done {
            let page = feed
                .page(heads.generation, feed_at, config.page_size)
                .await?;
            feed_done = page.len() < config.page_size;
            for entry in page {
                feed_at = i64::try_from(entry.position.seq).map_err(|_| CapturePgError::Corrupt)?;
                if feed_at > heads.feed_stable {
                    feed_after_stable += 1;
                }
                checker
                    .push_feed(entry)
                    .map_err(|_| CapturePgError::CheckerRefused)?;
            }
        }
        if !events_done {
            let page = events
                .page(heads.generation, events_at, config.page_size)
                .await?;
            events_done = page.len() < config.page_size;
            for event in page {
                events_at =
                    i64::try_from(event.position.seq).map_err(|_| CapturePgError::Corrupt)?;
                if events_at > heads.events_stable {
                    events_after_stable += 1;
                }
                checker
                    .push_event(event)
                    .map_err(|_| CapturePgError::CheckerRefused)?;
            }
        }
    }
    let report = checker.finish();
    let lag_entries = feed_after_stable.max(events_after_stable);
    let window_exceeds_lag = u64::try_from(config.window).is_ok_and(|w| w > lag_entries);

    let mut raise = Vec::new();
    let mut noted = Vec::new();
    let mut deferred = 0;
    let mut derived_without_projector = 0;
    let mut hold_feed = i64::MAX;
    let mut hold_events = i64::MAX;
    for finding in &report.findings {
        if finding.kind == FindingKind::DerivedWithoutProjector {
            derived_without_projector += 1;
            continue;
        }
        let feed_seq = finding
            .feed_position
            .and_then(|p| i64::try_from(p.seq).ok());
        let event_seq = finding
            .event_position
            .and_then(|p| i64::try_from(p.seq).ok());
        let is_new =
            feed_seq.is_some_and(|s| s > from.feed) || event_seq.is_some_and(|s| s > from.events);
        if !is_new {
            // Entirely behind the watermark: settled by an earlier pass.
            continue;
        }
        let is_settled = feed_seq.is_none_or(|s| s <= heads.feed_stable)
            && event_seq.is_none_or(|s| s <= heads.events_stable);
        if !is_settled {
            deferred += 1;
            continue;
        }
        let raises = match finding.kind {
            FindingKind::MissingStateEvent
            | FindingKind::OrphanStateEvent
            | FindingKind::VersionMismatch => true,
            FindingKind::OutOfWindow => window_exceeds_lag,
            _ => false,
        };
        if raises {
            if let Some(s) = feed_seq {
                hold_feed = hold_feed.min(s);
            }
            if let Some(s) = event_seq {
                hold_events = hold_events.min(s);
            }
            raise.push(finding.clone());
        } else {
            noted.push(finding.clone());
        }
    }
    // Held back below the earliest open finding, never behind where the pass started.
    let next = Watermarks {
        generation: heads.generation,
        feed: heads
            .feed_stable
            .min(hold_feed.saturating_sub(1))
            .max(from.feed),
        events: heads
            .events_stable
            .min(hold_events.saturating_sub(1))
            .max(from.events),
    };
    Ok(Scan {
        report,
        raise,
        noted,
        deferred,
        derived_without_projector,
        lag_entries,
        window_exceeds_lag,
        from,
        next,
    })
}

/// The CI mode: one full pass from the beginning with no lag, for a test that has stopped
/// writing. A non-empty report is a failing assertion.
#[derive(Clone, Debug)]
pub struct CaptureGate {
    exclusions: Exclusions,
    config: ScanConfig,
}

impl CaptureGate {
    /// A gate with no exclusions and the default page size and window.
    #[must_use]
    pub fn new() -> Self {
        Self {
            exclusions: Exclusions::new(),
            config: ScanConfig {
                lag_secs: 0,
                ..ScanConfig::default()
            },
        }
    }

    /// The same gate with `exclusions` (a derived or local-only entity is skipped).
    #[must_use]
    pub fn with_exclusions(mut self, exclusions: Exclusions) -> Self {
        self.exclusions = exclusions;
        self
    }

    /// The same gate with another page size and window.
    #[must_use]
    pub fn with_sizes(mut self, page_size: usize, window: usize) -> Self {
        self.config.page_size = page_size;
        self.config.window = window;
        self
    }

    /// Compares the whole change feed with the whole journal and returns the checker's report.
    ///
    /// # Errors
    ///
    /// As [`scan`].
    pub async fn run_once(&self, pool: &Pool) -> Result<Report, CapturePgError> {
        scan(
            pool,
            self.exclusions.clone(),
            Watermarks::none(),
            &self.config,
        )
        .await
        .map(|scan| scan.report)
    }
}

impl Default for CaptureGate {
    fn default() -> Self {
        Self::new()
    }
}
