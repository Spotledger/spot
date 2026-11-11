//! Counting inbound refusals per peer and window.
//!
//! An unauthenticated sender must not be able to make the receiver write one audit row per
//! request: that is an unbounded write amplification in a tamper-evident trail. The counter
//! keeps **one small window per peer** — not a map, so it cannot grow with anything a sender
//! controls — and the receiver writes:
//!
//! * one detail row for the first refusal of a window (the code of that refusal), and
//! * one summary row with `count`, the refusals **after** that first one, when the window ends,
//!   written by the first request that arrives after it. A window with only one refusal needs
//!   no summary row: its detail row already says everything.
//!
//! Time is always an argument (`now`, Unix milliseconds), as everywhere in this crate: nothing
//! here reads a clock, so a test (or a host replaying a trail) decides what "now" is. A count
//! that would overflow saturates instead, and a window only closes when time moves forward, so
//! a sender cannot open windows faster than the configured length by replaying requests.

use std::sync::{Mutex, MutexGuard};

use spot_tenant_link::ExternalPeerId;

/// What recording one refusal did to the peer's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The refusal opened the window: it is the one that carries a detail row.
    pub first: bool,
    /// A window that ended just before this refusal, with the number of refusals it counted
    /// **after** its detail row. The caller writes that one summary row. `None` when no
    /// window had ended or it had nothing beyond the detail row.
    pub closed: Option<u64>,
}

/// One peer's fixed-width window of inbound refusals, with bounded memory.
#[derive(Debug, Default)]
pub struct RefusalCounter {
    state: Mutex<Option<Window>>,
}

/// The currently open window: which peer it belongs to, when it opened and how many refusals
/// it has counted. Replaced (never accumulated) when it ends, so memory is constant.
#[derive(Clone, Debug)]
struct Window {
    /// The peer this window belongs to.
    peer: ExternalPeerId,
    /// When the window opened, Unix milliseconds.
    opened: u64,
    /// Refusals counted after the window's detail row; saturating.
    count: u64,
}

impl RefusalCounter {
    /// An empty counter. It is shared by every request for **one** peer: the receiver passes
    /// the peer's id, and a window opened for another id is closed rather than mixed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Closes the open window when `now` is at or past its end, returning how many refusals it
    /// counted after its detail row (only when that is greater than zero; a window with only
    /// the detail row needs no summary).
    ///
    /// This is what the receiver calls at the start of every request, so a summary row lands
    /// at the first request of the next window even when that request is accepted.
    pub fn close(&self, peer: &ExternalPeerId, now: u64, window_secs: u64) -> Option<u64> {
        let mut state = self.lock();
        let window = state.as_ref()?;
        if window.peer != *peer || !ended(window.opened, now, window_secs) {
            return None;
        }
        state
            .take()
            .filter(|closed| closed.count > 0)
            .map(|closed| closed.count)
    }

    /// Records one refusal for `peer` at `now`.
    ///
    /// The window opens on the first refusal (which the caller audits with its code) and counts
    /// the ones after it, saturating; a refusal at or after the window's end closes it and opens
    /// the next one, so the caller can write the summary even when it did not call
    /// [`RefusalCounter::close`] first.
    pub fn record(&self, peer: &ExternalPeerId, now: u64, window_secs: u64) -> Recorded {
        let mut state = self.lock();
        if let Some(window) = state.as_mut()
            && window.peer == *peer
            && !ended(window.opened, now, window_secs)
        {
            window.count = window.count.saturating_add(1);
            return Recorded {
                first: false,
                closed: None,
            };
        }
        let closed = match state.as_ref() {
            Some(window) if window.peer == *peer && window.count > 0 => Some(window.count),
            _ => None,
        };
        *state = Some(Window {
            peer: peer.clone(),
            opened: now,
            count: 0,
        });
        Recorded {
            first: true,
            closed,
        }
    }

    /// The lock, recovered if a panic poisoned it: a counter is bookkeeping, and refusing to
    /// count would let the sender write one row per request again (fail closed).
    fn lock(&self) -> MutexGuard<'_, Option<Window>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a window opened at `opened` (Unix milliseconds) has ended at `now`. `now` before
/// `opened` (a clock that went backwards) leaves the window open: it never closes early.
fn ended(opened: u64, now: u64, window_secs: u64) -> bool {
    now >= opened.saturating_add(window_secs.saturating_mul(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> ExternalPeerId {
        ExternalPeerId::new("acme-erp").unwrap()
    }

    #[test]
    fn a_window_counts_and_closes_once() {
        let counter = RefusalCounter::new();
        assert_eq!(
            counter.record(&peer(), 1_000, 60),
            Recorded {
                first: true,
                closed: None
            }
        );
        for _ in 0..999 {
            assert_eq!(
                counter.record(&peer(), 2_000, 60),
                Recorded {
                    first: false,
                    closed: None
                }
            );
        }
        assert_eq!(counter.close(&peer(), 60_999, 60), None);
        assert_eq!(counter.close(&peer(), 61_000, 60), Some(999));
        // Closed once only.
        assert_eq!(counter.close(&peer(), 61_001, 60), None);
    }

    #[test]
    fn a_count_saturates_instead_of_overflowing() {
        let counter = RefusalCounter::new();
        counter.record(&peer(), 0, 60);
        {
            let mut state = counter.lock();
            state.as_mut().unwrap().count = u64::MAX;
        }
        counter.record(&peer(), 1, 60);
        assert_eq!(counter.close(&peer(), 60_000, 60), Some(u64::MAX));
    }

    #[test]
    fn a_clock_that_goes_backwards_never_closes_the_window() {
        let counter = RefusalCounter::new();
        counter.record(&peer(), 10_000, 60);
        counter.record(&peer(), 10_001, 60);
        assert_eq!(counter.close(&peer(), 0, 60), None);
        assert_eq!(counter.close(&peer(), 9_999, 60), None);
        assert_eq!(counter.close(&peer(), 70_000, 60), Some(1));
    }

    #[test]
    fn another_peers_window_is_not_mixed_in() {
        let counter = RefusalCounter::new();
        counter.record(&peer(), 0, 60);
        let other = ExternalPeerId::new("someone-else").unwrap();
        // A counter is for one peer: another id does not inherit its count, and replaces it
        // (bounded memory) rather than accumulating a second window.
        assert_eq!(
            counter.record(&other, 120_000, 60),
            Recorded {
                first: true,
                closed: None
            }
        );
        counter.record(&other, 120_001, 60);
        assert_eq!(counter.close(&peer(), 120_000, 60), None);
        assert_eq!(counter.close(&other, 180_000, 60), Some(1));
    }
}
