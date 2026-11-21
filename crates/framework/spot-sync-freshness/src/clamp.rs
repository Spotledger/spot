//! Hub-side bound on a node's claimed clock offset (ADR-065 section 7, "Clocks").
//!
//! The stamp's `hub_offset_ms` is computed by the node, so a buggy or hostile node could claim an
//! offset that moves its `stamped_at` back before a mandatory event and escape a
//! `STAMP_VIOLATION`. The hub bounds the claim by what it observes itself: the document cannot
//! have been stamped after the hub received it, nor longer ago than the stamp's age bound.

use crate::error::FreshnessError;
use crate::stamp::SyncStamp;

/// Default clock-estimate uncertainty allowed past the hub receipt time: 2 seconds.
pub const DEFAULT_TOLERANCE_MS: i64 = 2_000;

/// The longest `max_age_ms` accepted: 366 days, exclusive.
pub const MAX_AGE_LIMIT_MS: i64 = 366 * 24 * 60 * 60 * 1_000;

/// The window a stamp's hub-timeline time must lie in, relative to the hub receipt time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    tolerance_ms: i64,
    max_age_ms: i64,
}

impl Bounds {
    /// Validated constructor.
    ///
    /// # Errors
    /// [`FreshnessError::BadBounds`] when either value is negative or `max_age_ms` is 366 days or
    /// more.
    pub fn new(tolerance_ms: i64, max_age_ms: i64) -> Result<Self, FreshnessError> {
        if tolerance_ms < 0 || !(0..MAX_AGE_LIMIT_MS).contains(&max_age_ms) {
            return Err(FreshnessError::BadBounds);
        }
        Ok(Self {
            tolerance_ms,
            max_age_ms,
        })
    }

    /// The default 2 second tolerance with the given age bound (the longest allowed offline
    /// period, a setting of the caller).
    ///
    /// # Errors
    /// As [`Bounds::new`].
    pub fn with_max_age(max_age_ms: i64) -> Result<Self, FreshnessError> {
        Self::new(DEFAULT_TOLERANCE_MS, max_age_ms)
    }

    /// Allowed uncertainty past the hub receipt time, milliseconds.
    #[must_use]
    pub fn tolerance_ms(&self) -> i64 {
        self.tolerance_ms
    }

    /// Oldest allowed stamp age at receipt, milliseconds.
    #[must_use]
    pub fn max_age_ms(&self) -> i64 {
        self.max_age_ms
    }
}

/// Why an offset was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClampReason {
    /// The claim put the stamp after `hub_received_at + tolerance`.
    InFuture,
    /// The claim put the stamp before `hub_received_at - max_age`.
    TooOld,
}

/// The result of bounding a claimed offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clamped {
    /// The offset the node claimed.
    pub claimed_offset_ms: i64,
    /// The offset to use (equal to the claim when it was not clamped).
    pub offset_ms: i64,
    /// True when the claim was replaced.
    pub was_clamped: bool,
    /// Why, when clamped.
    pub reason: Option<ClampReason>,
}

fn saturate(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

/// Bound `claimed_offset_ms` so that `node_stamped_at + offset` lies in
/// `[hub_received_at - max_age, hub_received_at + tolerance]`; an offset outside is replaced by the
/// nearest value inside. All arithmetic is exact (wide integers) and the result saturates to
/// `i64`; only a stamp time so extreme that the required offset does not fit `i64` can leave the
/// window, by saturation, and such a stamp is as unusable as any other garbage claim.
///
/// May saturate; use [`clamp_offset_checked`] at the hub boundary.
#[must_use]
pub fn clamp_offset(
    claimed_offset_ms: i64,
    node_stamped_at_millis: i64,
    hub_received_at_millis: i64,
    bounds: &Bounds,
) -> Clamped {
    let stamped = i128::from(node_stamped_at_millis);
    let effective = stamped + i128::from(claimed_offset_ms);
    let hi = i128::from(hub_received_at_millis) + i128::from(bounds.tolerance_ms);
    let lo = i128::from(hub_received_at_millis) - i128::from(bounds.max_age_ms);
    let (target, reason) = if effective > hi {
        (hi, Some(ClampReason::InFuture))
    } else if effective < lo {
        (lo, Some(ClampReason::TooOld))
    } else {
        return Clamped {
            claimed_offset_ms,
            offset_ms: claimed_offset_ms,
            was_clamped: false,
            reason: None,
        };
    };
    Clamped {
        claimed_offset_ms,
        offset_ms: saturate(target - stamped),
        was_clamped: true,
        reason,
    }
}

/// As [`clamp_offset`], but refuses what cannot be represented instead of saturating.
///
/// # Errors
/// [`FreshnessError::StampUnrepresentable`] when `node_stamped_at + claimed_offset` does not fit
/// `i64`, or when the offset (or the resulting hub-timeline time) needed to bring the stamp into
/// the window does not fit `i64`. Every `Ok` result puts the stamp inside the window.
pub fn clamp_offset_checked(
    claimed_offset_ms: i64,
    node_stamped_at_millis: i64,
    hub_received_at_millis: i64,
    bounds: &Bounds,
) -> Result<Clamped, FreshnessError> {
    let stamped = i128::from(node_stamped_at_millis);
    let effective = stamped + i128::from(claimed_offset_ms);
    if i64::try_from(effective).is_err() {
        return Err(FreshnessError::StampUnrepresentable);
    }
    let hi = i128::from(hub_received_at_millis) + i128::from(bounds.tolerance_ms);
    let lo = i128::from(hub_received_at_millis) - i128::from(bounds.max_age_ms);
    let (target, reason) = if effective > hi {
        (hi, ClampReason::InFuture)
    } else if effective < lo {
        (lo, ClampReason::TooOld)
    } else {
        return Ok(Clamped {
            claimed_offset_ms,
            offset_ms: claimed_offset_ms,
            was_clamped: false,
            reason: None,
        });
    };
    let offset_ms =
        i64::try_from(target - stamped).map_err(|_| FreshnessError::StampUnrepresentable)?;
    if i64::try_from(target).is_err() {
        return Err(FreshnessError::StampUnrepresentable);
    }
    Ok(Clamped {
        claimed_offset_ms,
        offset_ms,
        was_clamped: true,
        reason: Some(reason),
    })
}

/// A note for one hub audit entry about a rejected stamp: node id and amounts only, no payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectNote {
    /// Always `sync_freshness.stamp_unrepresentable`.
    pub code: &'static str,
    /// The node whose stamp was refused.
    pub node: String,
    /// The `stamped_at` the node claimed, milliseconds.
    pub claimed_stamped_at_millis: i64,
    /// The offset the node claimed, milliseconds.
    pub claimed_offset_ms: i64,
    /// When the hub received the stamp, milliseconds.
    pub hub_received_at_millis: i64,
    /// Why (a static reason).
    pub reason: &'static str,
}

/// The audit note for a stamp the hub refused as unrepresentable.
#[must_use]
pub fn reject_audit(stamp: &SyncStamp, hub_received_at_millis: i64) -> RejectNote {
    RejectNote {
        code: FreshnessError::StampUnrepresentable.code(),
        node: stamp.node.clone(),
        claimed_stamped_at_millis: stamp.stamped_at_millis,
        claimed_offset_ms: stamp.hub_offset_ms,
        hub_received_at_millis,
        reason: "the stamp time and offset cannot be brought into the window within i64",
    }
}

/// A note for one hub audit entry: node id and amounts only, no payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditNote {
    /// Always `sync_freshness.offset_clamped`.
    pub code: &'static str,
    /// The node whose claim was replaced.
    pub node: String,
    /// What the node claimed, milliseconds.
    pub claimed_offset_ms: i64,
    /// What the hub applied, milliseconds.
    pub applied_offset_ms: i64,
    /// Why.
    pub reason: ClampReason,
}

/// The audit note for a clamped stamp; `None` when the claim was honest.
#[must_use]
pub fn stamp_audit(stamp: &SyncStamp, clamped: &Clamped) -> Option<AuditNote> {
    if !clamped.was_clamped {
        return None;
    }
    Some(AuditNote {
        code: "sync_freshness.offset_clamped",
        node: stamp.node.clone(),
        claimed_offset_ms: clamped.claimed_offset_ms,
        applied_offset_ms: clamped.offset_ms,
        reason: clamped.reason?,
    })
}
