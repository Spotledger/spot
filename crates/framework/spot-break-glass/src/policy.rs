//! The tenant's break-glass policy: the limits a request and its approvals must satisfy.
//!
//! The policy is resolved by the host from the tenant's typed settings (`spot-settings`,
//! WP-ADM-07) and the deployment's own ceiling; this crate holds the resolved numbers and
//! the one rule that matters, that a tenant setting may narrow a deployment ceiling but
//! never widen it. No policy is read from the environment and no clock is read anywhere.

use crate::error::BreakGlassError;

/// The resolved break-glass policy for one tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Whether the tenant has opted in to break-glass access at all. A tenant that has
    /// not opted in can never open a session.
    pub opted_in: bool,
    /// Whether the tenant's policy additionally requires a customer consent receipt on
    /// top of the two vendor-side approvals.
    pub require_customer_consent: bool,
    /// How many distinct vendor-side approvals a session needs. Two by default.
    pub required_approvers: usize,
    /// The deployment's ceiling on a break-glass window, in seconds.
    pub max_duration_secs: i64,
    /// The tenant's `spot-settings` ceiling on a break-glass window, in seconds. The
    /// effective cap is the smaller of this and [`Policy::max_duration_secs`].
    pub settings_max_duration_secs: i64,
    /// The maximum size of one recording, in bytes, before the truncation marker.
    pub max_recording_bytes: u64,
}

impl Default for Policy {
    /// The shipped defaults: opted out, customer consent required, two approvers, a four
    /// hour window capped identically by the tenant setting, and a 64 MiB recording.
    fn default() -> Self {
        Self {
            opted_in: false,
            require_customer_consent: true,
            required_approvers: 2,
            max_duration_secs: 4 * 3600,
            settings_max_duration_secs: 4 * 3600,
            max_recording_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Policy {
    /// The effective maximum window, in seconds: the deployment ceiling capped by the
    /// tenant setting (the smaller of the two).
    #[must_use]
    pub fn max_duration(&self) -> i64 {
        self.max_duration_secs.min(self.settings_max_duration_secs)
    }

    /// Whether the policy is internally usable: opted in, at least one approver (two is
    /// the contract, but a tenant may be stricter), a positive window cap and a
    /// non-zero recording cap.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::NotOptedIn`] when the tenant has not opted in;
    /// [`BreakGlassError::DurationOverPolicy`] when a cap is not positive;
    /// [`BreakGlassError::RecordingFull`] when the recording cap is zero.
    pub fn validate(&self) -> Result<(), BreakGlassError> {
        if !self.opted_in {
            return Err(BreakGlassError::NotOptedIn);
        }
        // Break-glass needs two distinct vendor-side people (WP-ADM-11); a policy cannot
        // lower that bar.
        if self.required_approvers < 2 {
            return Err(BreakGlassError::NotEnoughApprovers {
                have: self.required_approvers,
                need: 2,
            });
        }
        if self.max_duration() <= 0 {
            return Err(BreakGlassError::DurationOverPolicy {
                asked: 0,
                max: self.max_duration(),
            });
        }
        if self.max_recording_bytes == 0 {
            return Err(BreakGlassError::RecordingFull);
        }
        Ok(())
    }
}
