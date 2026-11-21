//! Typed errors for `spot-sync-freshness`: stable `sync_freshness.*` codes.
//!
//! Callers match on [`FreshnessError::code`], never on the message. Every code is catalogued in
//! `crates/framework/spot-errors/catalogue/sync_freshness.toml`.

/// What can go wrong in the clock offset estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FreshnessError {
    /// `hub_offset_estimate` was given no samples.
    #[error("no clock samples to estimate the hub offset from")]
    NoSamples,

    /// A sample's timestamps cannot come from one request and response: the response was
    /// received before the request was sent, or the hub answered before it received.
    #[error("clock sample {index} is inconsistent: a response cannot precede its request")]
    BadSample {
        /// Position of the offending sample in the input.
        index: usize,
    },

    /// `Bounds::new` was given a negative value or an age bound of 366 days or more.
    #[error("clock bounds are invalid: values must be non-negative and max age under 366 days")]
    BadBounds,

    /// The clamp result was computed for a different stamp than the one it is applied to.
    #[error(
        "the clamp was computed for another stamp: its claimed offset differs from this stamp's"
    )]
    OffsetNotClamped,

    /// The stamp's time plus its offset, or the offset needed to bring it into the window, does
    /// not fit a 64-bit millisecond count. Garbage or an attack; the hub refuses the stamp.
    #[error("the stamp cannot be placed on the hub timeline: its time or offset is out of range")]
    StampUnrepresentable,
}

impl FreshnessError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoSamples => "sync_freshness.no_samples",
            Self::BadSample { .. } => "sync_freshness.bad_sample",
            Self::BadBounds => "sync_freshness.bad_bounds",
            Self::OffsetNotClamped => "sync_freshness.offset_not_clamped",
            Self::StampUnrepresentable => "sync_freshness.stamp_unrepresentable",
        }
    }
}
