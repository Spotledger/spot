//! The hub-side stale-document check (ADR-065 section 5).

use serde::{Deserialize, Serialize};

use crate::clamp::{Bounds, Clamped, clamp_offset_checked};
use crate::error::FreshnessError;
use crate::stamp::SyncStamp;

/// A mandatory record version the hub published. All times are on the **hub** timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandatoryEvent {
    /// Journal position of the event.
    pub position: u64,
    /// When the hub published it, milliseconds since the epoch.
    pub published_at: i64,
    /// When it comes into force, if it is effective-dated.
    pub effective_from: Option<i64>,
    /// The record version, for the finding.
    pub record: String,
}

/// The kind of a finding. Only one exists today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingKind {
    /// `STAMP_VIOLATION`: the document was posted without a rule already in force.
    StampViolation,
}

/// A stale-document finding for the finding queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The kind of finding.
    pub kind: FindingKind,
    /// The document that was posted.
    pub document: String,
    /// The mandatory record the node posted without.
    pub mandatory_record: String,
    /// The node that posted.
    pub node: String,
    /// Time between the record being published and the document being stamped (hub timeline),
    /// milliseconds.
    pub elapsed_millis: u64,
    /// True when the stamp's claimed offset was bounded by the hub before the check.
    #[serde(default)]
    pub clamped: bool,
    /// The hub-timeline stamp time the check used, milliseconds since the epoch.
    #[serde(default)]
    pub effective_stamped_at_millis: i64,
}

/// A posting time on the **hub** timeline. It has no public constructor from a bare `i64` and
/// none from a [`SyncStamp`], so a node's claimed offset cannot be turned into one: the only
/// routes are [`HubPostedAt::from_clamped`] (the hub's bounded offset) and
/// [`HubPostedAt::hub_received`] (a document the hub itself posts).
///
/// ```compile_fail
/// use spot_sync_freshness::HubPostedAt;
/// let _ = HubPostedAt(1_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HubPostedAt(i64);

impl HubPostedAt {
    /// The stamp's `stamped_at` plus the **clamped** offset.
    ///
    /// # Errors
    /// [`FreshnessError::StampUnrepresentable`] when the sum does not fit `i64`.
    /// [`FreshnessError::OffsetNotClamped`] when `clamped.claimed_offset_ms` differs from the
    /// stamp's own `hub_offset_ms`, i.e. the clamp was computed for another stamp.
    pub fn from_clamped(stamp: &SyncStamp, clamped: &Clamped) -> Result<Self, FreshnessError> {
        if clamped.claimed_offset_ms != stamp.hub_offset_ms {
            return Err(FreshnessError::OffsetNotClamped);
        }
        stamp
            .stamped_at_millis
            .checked_add(clamped.offset_ms)
            .map(Self)
            .ok_or(FreshnessError::StampUnrepresentable)
    }

    /// For a document the hub posts itself: the hub's own receipt time.
    #[must_use]
    pub fn hub_received(received_at_millis: i64) -> Self {
        Self(received_at_millis)
    }

    /// Milliseconds since the epoch on the hub timeline.
    #[must_use]
    pub fn millis(self) -> i64 {
        self.0
    }
}

fn in_force_at(event: &MandatoryEvent, posted_at: HubPostedAt) -> bool {
    event.effective_from.is_none_or(|e| e <= posted_at.0)
}

/// One finding per mandatory event that was published at or before the skew-corrected
/// `stamped_at`, is effective at or before `document_posted_at`, and lies beyond the
/// stamp's `applied_prefix`. Ordered by position.
///
/// `document_posted_at` is the posting time **on the hub timeline**; it can only be built from a
/// clamped offset or the hub's own receipt time ([`HubPostedAt`]).
///
/// The stamp's `hub_offset_ms` is **trusted** here: it must already have been bounded by
/// [`clamp_offset_checked`]. For an offset straight from a node use [`find_violations_clamped`].
#[must_use]
pub fn find_violations(
    document: &str,
    stamp: &SyncStamp,
    document_posted_at: HubPostedAt,
    mandatory_events: &[MandatoryEvent],
) -> Vec<Finding> {
    find_inner(document, stamp, document_posted_at, mandatory_events, false)
}

/// As [`find_violations`], but first bounds the stamp's claimed offset with [`clamp_offset_checked`]
/// against the hub's own receipt time `hub_received_at_millis`; each finding records whether the
/// offset was clamped and the effective hub-time stamp used. The posting time is built from the
/// same clamp, so it cannot differ from what the check used.
///
/// # Errors
/// [`FreshnessError::StampUnrepresentable`] when the stamp cannot be placed in the window; the
/// hub refuses it (see `reject_audit`).
pub fn find_violations_clamped(
    document: &str,
    stamp: &SyncStamp,
    hub_received_at_millis: i64,
    bounds: &Bounds,
    mandatory_events: &[MandatoryEvent],
) -> Result<Vec<Finding>, FreshnessError> {
    let c = clamp_offset_checked(
        stamp.hub_offset_ms,
        stamp.stamped_at_millis,
        hub_received_at_millis,
        bounds,
    )?;
    let mut trusted = stamp.clone();
    trusted.hub_offset_ms = c.offset_ms;
    let posted_at = HubPostedAt::from_clamped(stamp, &c)?;
    Ok(find_inner(
        document,
        &trusted,
        posted_at,
        mandatory_events,
        c.was_clamped,
    ))
}

fn find_inner(
    document: &str,
    stamp: &SyncStamp,
    document_posted_at: HubPostedAt,
    mandatory_events: &[MandatoryEvent],
    clamped: bool,
) -> Vec<Finding> {
    let stamped = stamp.claimed_hub_time();
    let mut found: Vec<&MandatoryEvent> = mandatory_events
        .iter()
        .filter(|m| {
            m.published_at <= stamped
                && in_force_at(m, document_posted_at)
                && m.position > stamp.applied_prefix
        })
        .collect();
    found.sort_by_key(|m| m.position);
    found
        .into_iter()
        .map(|m| Finding {
            kind: FindingKind::StampViolation,
            document: document.to_owned(),
            mandatory_record: m.record.clone(),
            node: stamp.node.clone(),
            elapsed_millis: u64::try_from(stamped.saturating_sub(m.published_at)).unwrap_or(0),
            clamped,
            effective_stamped_at_millis: stamped,
        })
        .collect()
}

/// The retroactive corrections for the report: events published **after** the document was
/// stamped whose effective time is at or before the posting time. They raise no finding.
/// Ordered by position.
#[must_use]
pub fn list_retroactive<'a>(
    stamp: &SyncStamp,
    document_posted_at: HubPostedAt,
    mandatory_events: &'a [MandatoryEvent],
) -> Vec<&'a MandatoryEvent> {
    let stamped = stamp.claimed_hub_time();
    let mut out: Vec<&MandatoryEvent> = mandatory_events
        .iter()
        .filter(|m| m.published_at > stamped && in_force_at(m, document_posted_at))
        .collect();
    out.sort_by_key(|m| m.position);
    out
}
