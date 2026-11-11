//! The hub clock offset estimate (ADR-065 section 7).

use crate::error::FreshnessError;

/// One request/response exchange, four wall-clock timestamps in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    /// Node clock when the request was sent.
    pub node_sent: i64,
    /// Hub clock when the request was received.
    pub hub_received: i64,
    /// Hub clock when the response was sent.
    pub hub_sent: i64,
    /// Node clock when the response was received.
    pub node_received: i64,
}

/// The estimated difference `hub clock - node clock` and how far it may be off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Hub minus node, milliseconds.
    pub offset_ms: i64,
    /// Half the round trip of the sample used, rounded up: the true offset lies within it
    /// (assuming symmetric path delay).
    pub uncertainty_ms: u64,
}

/// NTP-style estimate: per sample `offset = ((t1 - t0) + (t2 - t3)) / 2` and
/// `delay = (t3 - t0) - (t2 - t1)`; the sample with the smallest delay wins (earliest on a tie).
///
/// # Errors
/// [`FreshnessError::NoSamples`] for an empty slice; [`FreshnessError::BadSample`] when a sample
/// has a negative round trip or a negative hub processing time.
pub fn hub_offset_estimate(samples: &[ClockSample]) -> Result<Estimate, FreshnessError> {
    if samples.is_empty() {
        return Err(FreshnessError::NoSamples);
    }
    let mut best: Option<(i128, i128)> = None; // (delay, offset)
    for (index, s) in samples.iter().enumerate() {
        let (t0, t1, t2, t3) = (
            i128::from(s.node_sent),
            i128::from(s.hub_received),
            i128::from(s.hub_sent),
            i128::from(s.node_received),
        );
        let hub_time = t2 - t1;
        let delay = (t3 - t0) - hub_time;
        if hub_time < 0 || delay < 0 {
            return Err(FreshnessError::BadSample { index });
        }
        let offset = ((t1 - t0) + (t2 - t3)).div_euclid(2);
        if best.is_none_or(|(d, _)| delay < d) {
            best = Some((delay, offset));
        }
    }
    let (delay, offset) = best.ok_or(FreshnessError::NoSamples)?;
    Ok(Estimate {
        offset_ms: i64::try_from(offset).unwrap_or(if offset < 0 { i64::MIN } else { i64::MAX }),
        uncertainty_ms: u64::try_from((delay + 1) / 2).unwrap_or(u64::MAX),
    })
}
