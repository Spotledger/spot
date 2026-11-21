//! Counting refusals so that one authenticated node cannot flood the audit log.
//!
//! Every refusal is audited (the rule from WP-EDG-03.T4 is unchanged), but a node that asks in
//! a loop would otherwise write one audit record per attempt. So the service keeps one window
//! per `(node, series, reason)` — [`DEFAULT_REFUSAL_WINDOW`](crate::DEFAULT_REFUSAL_WINDOW) by
//! default, measured on the instants the caller passes and never on a clock read:
//!
//! * the first refusal of a window writes its record at once (a lone refusal is never silent),
//!   with `count = 1`;
//! * the rest of the window only increments the count, writing nothing;
//! * when the window closes, one aggregate record carries the window's **total** count.
//!
//! So a flood of ten thousand refusals costs two records for that window, and the audit still
//! says how many there were. A window closes when the instant passed in reaches its end; an
//! instant inside it (even one earlier than its start) is counted, so a caller cannot
//! manufacture a window and a record per attempt. The map is bounded
//! ([`MAX_REFUSAL_WINDOWS`](self::MAX_REFUSAL_WINDOWS)); when it is full the oldest window is
//! closed and evicted, so its count is not lost. A restart forgets the open windows, which
//! loses counting detail but never a refusal: the next one opens a window and is written.

use std::collections::BTreeMap;

use spot_sync_protocol::NodeId;
use time::{Duration, OffsetDateTime};

/// The most refusal windows the service keeps at once.
///
/// A capacity limit: one window is 3 strings and a counter, and the number of windows is
/// bounded by nodes × series × reasons in any case. Past it the oldest window is closed and
/// dropped, so memory never grows with a flood.
pub const MAX_REFUSAL_WINDOWS: usize = 4_096;

/// Why a refusal was written: the audited vocabulary of refusal reasons.
///
/// The spelling of each one is a wire-visible audit value and does not change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RefusalReason {
    /// The payload named another node than the authenticated one.
    NodeMismatch,
    /// The node is suspended, decommissioned or unknown.
    NodeNotActive,
    /// The standing source could not answer.
    StandingUnavailable,
    /// The node is not entitled to the series.
    SeriesNotEntitled,
    /// The service already tracks as many series as its policy allows.
    SeriesCapReached,
    /// The size is zero or above the series maximum.
    SizeRefused,
    /// The instant arithmetic overflowed, so the rate limit cannot be decided.
    TimeInvalid,
    /// The node was granted a range of the series too recently.
    TooSoon,
    /// The series has no numbers left for that size.
    SeriesExhausted,
    /// The allocator failed for another reason.
    AllocatorFailed,
    /// The log could not record the grant.
    LogFailed,
}

impl RefusalReason {
    /// The stable audit spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NodeMismatch => "node_mismatch",
            Self::NodeNotActive => "node_not_active",
            Self::StandingUnavailable => "standing_unavailable",
            Self::SeriesNotEntitled => "series_not_entitled",
            Self::SeriesCapReached => "series_cap_reached",
            Self::SizeRefused => "size_refused",
            Self::TimeInvalid => "time_invalid",
            Self::TooSoon => "too_soon",
            Self::SeriesExhausted => "series_exhausted",
            Self::AllocatorFailed => "allocator_failed",
            Self::LogFailed => "log_failed",
        }
    }
}

/// One `(node, series, reason)` window.
#[derive(Clone, Copy, Debug)]
struct Window {
    /// The instant the window opened.
    start: OffsetDateTime,
    /// How many refusals it has counted.
    count: u64,
}

/// What the service must write after booking one refusal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Booked {
    /// The total of a window that just closed and needs its aggregate record, when it held more
    /// than the single record already written for it.
    pub(crate) closed: Option<u64>,
    /// The count for the new window's first record, when one must be written now.
    pub(crate) opened: Option<u64>,
}

/// The per-`(node, series, reason)` refusal windows.
#[derive(Debug)]
pub(crate) struct RefusalWindows {
    window: Duration,
    open: BTreeMap<(String, String, RefusalReason), Window>,
}

impl RefusalWindows {
    /// Empty windows, each `window` long.
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            window,
            open: BTreeMap::new(),
        }
    }

    /// Books one refusal of `reason` by `node` in `series` at `at`, and says what to write.
    pub(crate) fn book(
        &mut self,
        node: &NodeId,
        series: &str,
        reason: RefusalReason,
        at: OffsetDateTime,
    ) -> Booked {
        let key = (node.as_str().to_owned(), series.to_owned(), reason);
        if let Some(window) = self.open.get_mut(&key) {
            // Inside the window: counted, not written. The window is decided by its *end* only,
            // so a caller that passes an earlier instant cannot manufacture a new window (and a
            // record) per attempt. A window whose end overflows the date range cannot be shown
            // to have closed, so it keeps counting instead of opening a new one every time.
            let ends = window.start.checked_add(self.window);
            if ends.is_none_or(|ends| at < ends) {
                window.count = window.count.saturating_add(1);
                return Booked::default();
            }
            let previous = *window;
            self.open.remove(&key);
            let closed = (previous.count > 1).then_some(previous.count);
            self.open.insert(
                key,
                Window {
                    start: at,
                    count: 1,
                },
            );
            return Booked {
                closed,
                opened: Some(1),
            };
        }
        // A new window, evicting the oldest one when the map is full: its count is kept.
        let mut closed = None;
        if self.open.len() >= MAX_REFUSAL_WINDOWS
            && let Some((oldest, window)) = self
                .open
                .iter()
                .min_by_key(|(_, window)| window.start)
                .map(|(key, window)| (key.clone(), *window))
        {
            self.open.remove(&oldest);
            if window.count > 1 {
                closed = Some(window.count);
            }
        }
        self.open.insert(
            key,
            Window {
                start: at,
                count: 1,
            },
        );
        Booked {
            closed,
            opened: Some(1),
        }
    }

    /// How many windows are open (tests and the hub's diagnostics).
    pub(crate) fn len(&self) -> usize {
        self.open.len()
    }
}
