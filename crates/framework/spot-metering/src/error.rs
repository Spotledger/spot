//! Stable error codes for metering, matching the `metering.*` family of the error
//! registry. Catalogued in `crates/framework/spot-errors/catalogue/metering.toml`.

/// The typed errors of the metering crate.
///
/// Like the other framework crates, time-check and quota *outcomes* are not errors —
/// [`crate::decision::QuotaDecision`] is the value. These cover the cases where the
/// metering machinery itself is misconfigured or asked to do something impossible.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MeteringError {
    /// Guard is explicitly bound to another tenant.
    #[error("metering tenant mismatch")]
    TenantMismatch,
    /// An event belongs to a period the store no longer accepts: its tombstones were
    /// removed by compaction and the period is past the retention horizon, so counting
    /// it could double count a replay. Never retried — the fact has left the window.
    #[error("event {id} belongs to period {period}, which is past the retention horizon")]
    LateBeyondHorizon {
        /// The event's id, for the producer to find the fact.
        id: String,
        /// The period the event would have landed in.
        period: String,
    },
    /// One `add_events` call carried more events than [`crate::MAX_BATCH_EVENTS`], so it
    /// would hold one transaction for longer than the store's bound.
    #[error("a batch of {size} events exceeds the bound of {max} events per call")]
    BatchTooLarge {
        /// The size of the refused batch.
        size: usize,
        /// The documented bound.
        max: usize,
    },
    /// A retention horizon is shorter than the store accepts. Compaction refuses it
    /// instead of purging: a too-short horizon turns events a producer may still retry
    /// into refusals, which is unbilled usage rather than free storage.
    #[error("a retention horizon of {periods} periods is shorter than the minimum of {min}")]
    RetentionHorizonTooShort {
        /// The horizon the policy asked for.
        periods: u32,
        /// The shortest horizon compaction accepts.
        min: u32,
    },
    /// A counter cannot represent the exact sum.
    #[error("meter counter overflow")]
    CounterOverflow,
    /// A period spec or quota policy referenced an IANA zone the compiled tzdb does
    /// not know.
    #[error("unknown IANA time zone {zone:?}")]
    UnknownZone {
        /// The zone name as given.
        zone: String,
    },
    /// A `PeriodSpec` or `QuotaPolicy` carried a fraction outside 0..=10,000 bps.
    #[error("basis points out of range: {bps} (must be 0..=10000)")]
    BasisPointsOutOfRange {
        /// The out-of-range value.
        bps: u64,
    },
    /// A period spec itself was inconsistent (an anchor day outside 1..=31, or an
    /// un-representable date).
    #[error("invalid period spec: {reason}")]
    InvalidPeriodSpec {
        /// What was wrong.
        reason: String,
    },
    /// A distinct-set event arrived without a member, or a counter/gauge event
    /// carried one; the sink refused it before it could corrupt a total.
    #[error("event {id} does not match its meter kind {meter}")]
    MalformedEvent {
        /// The event's id, for the producer to find the bug.
        id: String,
        /// The meter's name.
        meter: String,
    },
    /// The store was asked for a period whose spec names a zone it cannot resolve,
    /// or an instant that is not representable.
    #[error("period at instant {at} cannot be resolved in zone {zone:?}")]
    PeriodUnresolvable {
        /// The instant asked about.
        at: i64,
        /// The zone that failed.
        zone: String,
    },
    /// The backing store could not be reached or failed while running a statement
    /// (class `unavailable`, retryable). `cause` is a stable short code such as
    /// `pool_exhausted`, `connection_lost` or `timeout`; never a SQLSTATE, a host, a
    /// value or driver text.
    #[error("the metering store is unavailable ({cause})")]
    StoreUnavailable {
        /// A stable short cause code.
        cause: &'static str,
    },
}

impl MeteringError {
    /// Whether the same call may succeed if tried again later: true only for an outage
    /// of the backing store.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::StoreUnavailable { .. })
    }

    /// The stable machine-readable code for logs, API responses and metrics.
    ///
    /// Codes are `metering.<snake_case>` and never change in a breaking way.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TenantMismatch => "metering.tenant_mismatch",
            Self::LateBeyondHorizon { .. } => "metering.late_beyond_horizon",
            Self::BatchTooLarge { .. } => "metering.batch_too_large",
            Self::CounterOverflow => "metering.counter_overflow",
            Self::UnknownZone { .. } => "metering.unknown_zone",
            Self::BasisPointsOutOfRange { .. } => "metering.basis_points_out_of_range",
            Self::InvalidPeriodSpec { .. } => "metering.invalid_period_spec",
            Self::MalformedEvent { .. } => "metering.malformed_event",
            Self::PeriodUnresolvable { .. } => "metering.period_unresolvable",
            Self::RetentionHorizonTooShort { .. } => "metering.retention_horizon_too_short",
            Self::StoreUnavailable { .. } => "metering.store_unavailable",
        }
    }
}

impl std::fmt::Debug for MeteringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Errors never carry meter values or member digests; Debug stays deliberate
        // and short, echoing nothing new.
        f.write_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metering_codes_are_stable_and_cover_every_variant() {
        let cases: Vec<(MeteringError, &str)> = vec![
            (
                MeteringError::UnknownZone {
                    zone: "Mars/Olympus".to_owned(),
                },
                "metering.unknown_zone",
            ),
            (
                MeteringError::BasisPointsOutOfRange { bps: 20_000 },
                "metering.basis_points_out_of_range",
            ),
            (
                MeteringError::InvalidPeriodSpec {
                    reason: "anchor".to_owned(),
                },
                "metering.invalid_period_spec",
            ),
            (
                MeteringError::MalformedEvent {
                    id: "e1".to_owned(),
                    meter: "active_users".to_owned(),
                },
                "metering.malformed_event",
            ),
            (
                MeteringError::PeriodUnresolvable {
                    at: 0,
                    zone: "?".to_owned(),
                },
                "metering.period_unresolvable",
            ),
            (
                MeteringError::StoreUnavailable { cause: "timeout" },
                "metering.store_unavailable",
            ),
            (
                MeteringError::LateBeyondHorizon {
                    id: "e1".to_owned(),
                    period: "2026-01".to_owned(),
                },
                "metering.late_beyond_horizon",
            ),
            (
                MeteringError::RetentionHorizonTooShort { periods: 1, min: 2 },
                "metering.retention_horizon_too_short",
            ),
            (
                MeteringError::BatchTooLarge {
                    size: 10_001,
                    max: 10_000,
                },
                "metering.batch_too_large",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "code drifted for {error}");
        }
    }

    #[test]
    fn a_store_outage_is_retryable_and_its_text_carries_only_the_cause_code() {
        let error = MeteringError::StoreUnavailable {
            cause: "connection_lost",
        };
        assert!(error.is_retryable());
        assert_eq!(
            error.to_string(),
            "the metering store is unavailable (connection_lost)"
        );
        assert_eq!(format!("{error:?}"), error.to_string());
        assert!(!MeteringError::CounterOverflow.is_retryable());
        assert!(
            !MeteringError::InvalidPeriodSpec {
                reason: "x".to_owned()
            }
            .is_retryable()
        );
    }
}
