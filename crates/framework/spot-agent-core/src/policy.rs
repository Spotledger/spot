//! The decision policy: freshness and lifetime bounds, the risk-to-approval rules, the
//! requester/hardware requirements, and the critical-operation clearance boundary.
//!
//! The risk level comes from the operation catalogue ([`spot_mgmt_ops::Risk`]); this
//! module only attaches approval rules to it. `Read`, `Low` and `Medium` need one valid
//! administrator signature; `High` needs two distinct administrators; `Critical` needs
//! the same two-person approval plus a clearance from the injected [`CriticalContext`]
//! (a configured delay that has elapsed, or a live break-glass grant handed over by
//! ADM-11). The mapping is exhaustive over [`Risk`], so a new risk level cannot be added
//! without a compile error here and a decision in review.

use spot_mgmt_ops::{OpId, Risk};
use spot_signed_command::{KeyId, VerifyPolicy};

use crate::error::Refusal;

/// The default clock-skew tolerance, 60 seconds (matches the signed-command default).
pub const DEFAULT_MAX_SKEW: i64 = 60;
/// The default maximum command lifetime, 15 minutes (matches the signed-command default).
pub const DEFAULT_MAX_LIFETIME: i64 = 900;

/// How a risk level maps to a required approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// One valid administrator signature.
    Single,
    /// Two distinct valid administrators.
    TwoPerson,
    /// Two distinct valid administrators plus a critical clearance.
    Critical,
}

/// The agent's decision policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Clock-skew tolerance on the issue side, seconds (the signed-command default).
    pub max_skew: i64,
    /// The longest validity window accepted, seconds.
    pub max_lifetime: i64,
    /// When set, the command's issuer may not be one of the approving signers.
    pub requester_cannot_approve: bool,
    /// When set, every signer of a dangerous operation must be a hardware-backed key.
    pub require_hardware_keys: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_skew: DEFAULT_MAX_SKEW,
            max_lifetime: DEFAULT_MAX_LIFETIME,
            requester_cannot_approve: true,
            require_hardware_keys: true,
        }
    }
}

impl Policy {
    /// The signed-command verification policy with `min_signatures` distinct signers.
    #[must_use]
    pub fn verify_policy(&self, min_signatures: usize) -> VerifyPolicy {
        VerifyPolicy {
            max_skew: self.max_skew,
            max_lifetime: self.max_lifetime,
            min_signatures,
        }
    }

    /// The approval a risk level requires. Exhaustive over [`Risk`].
    #[must_use]
    pub fn approval_for(risk: Risk) -> Approval {
        match risk {
            Risk::Read | Risk::Low | Risk::Medium => Approval::Single,
            Risk::High => Approval::TwoPerson,
            Risk::Critical => Approval::Critical,
        }
    }

    /// How many distinct valid administrator signatures a risk level requires.
    #[must_use]
    pub fn required_distinct(risk: Risk) -> usize {
        match Self::approval_for(risk) {
            Approval::Single => 1,
            Approval::TwoPerson | Approval::Critical => 2,
        }
    }

    /// Whether a risk level needs a critical clearance.
    #[must_use]
    pub fn is_critical(risk: Risk) -> bool {
        matches!(Self::approval_for(risk), Approval::Critical)
    }
}

/// The facts a [`CriticalContext`] decides on. Borrowed, so the trait stays allocation
/// free and the context sees only what it needs.
#[derive(Clone, Copy, Debug)]
pub struct CriticalRequest<'a> {
    /// The agent the command is for.
    pub agent_id: &'a str,
    /// The command's random id.
    pub command_id: &'a str,
    /// The operation id.
    pub op: &'a OpId,
    /// The command's digest (the hash of its signed bytes).
    pub command_digest: [u8; 32],
    /// The issuing administrator.
    pub requester: &'a KeyId,
    /// The command's issue instant, for a configured-delay check.
    pub issued_at: i64,
}

/// The clearance a critical operation presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clearance {
    /// A configured delay has elapsed; `ready_at` is the earliest instant it allowed.
    Delay {
        /// The instant the delay allowed, Unix seconds.
        ready_at: i64,
    },
    /// A live break-glass grant covers the operation.
    BreakGlass {
        /// The grant identifier handed over by ADM-11.
        grant: String,
    },
}

/// The boundary a critical operation must clear. The brief hands this over to ADM-11
/// (break-glass) via a trait; the host injects the implementation. Implementations must
/// be read-only (they may be consulted by both the broker and the executor for the same
/// command) and must fail closed.
pub trait CriticalContext: Send + Sync {
    /// The clearance at `now`, or a refusal.
    ///
    /// # Errors
    /// [`Refusal::CriticalNotCleared`] when neither a delay has elapsed nor a break-glass
    /// grant is live.
    fn clearance(&self, request: &CriticalRequest<'_>, now: i64) -> Result<Clearance, Refusal>;
}

/// A critical context that clears a command once `seconds` have passed since its issue
/// instant: the "configured delay" of the brief, with no break-glass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayOnly {
    /// The configured delay in seconds. `0` clears as soon as the command is issued.
    pub seconds: i64,
}

impl DelayOnly {
    /// A context with the given delay.
    #[must_use]
    pub fn new(seconds: i64) -> Self {
        Self { seconds }
    }
}

impl CriticalContext for DelayOnly {
    fn clearance(&self, request: &CriticalRequest<'_>, now: i64) -> Result<Clearance, Refusal> {
        let ready_at = request.issued_at.saturating_add(self.seconds);
        if now >= ready_at {
            Ok(Clearance::Delay { ready_at })
        } else {
            Err(Refusal::CriticalNotCleared {
                op: request.op.as_str().to_owned(),
            })
        }
    }
}

/// The fail-closed default: a critical operation is never cleared. Used until a host wires
/// a delay or break-glass context in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoCritical;

impl CriticalContext for NoCritical {
    fn clearance(&self, request: &CriticalRequest<'_>, _now: i64) -> Result<Clearance, Refusal> {
        Err(Refusal::CriticalNotCleared {
            op: request.op.as_str().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn approval_rules_are_exhaustive_and_ordered() {
        assert_eq!(Policy::approval_for(Risk::Read), Approval::Single);
        assert_eq!(Policy::approval_for(Risk::Low), Approval::Single);
        assert_eq!(Policy::approval_for(Risk::Medium), Approval::Single);
        assert_eq!(Policy::approval_for(Risk::High), Approval::TwoPerson);
        assert_eq!(Policy::approval_for(Risk::Critical), Approval::Critical);
        assert_eq!(Policy::required_distinct(Risk::Low), 1);
        assert_eq!(Policy::required_distinct(Risk::High), 2);
        assert_eq!(Policy::required_distinct(Risk::Critical), 2);
        assert!(Policy::is_critical(Risk::Critical));
        assert!(!Policy::is_critical(Risk::High));
    }

    #[test]
    fn delay_only_clears_after_the_configured_seconds() {
        let op = OpId::new("server.service.restart").unwrap();
        let requester = KeyId::from_public_key(&[7u8; 32]);
        let request = CriticalRequest {
            agent_id: "agent-1",
            command_id: "c1",
            op: &op,
            command_digest: [0u8; 32],
            requester: &requester,
            issued_at: 1_000,
        };
        let delay = DelayOnly::new(300);
        assert_eq!(
            delay.clearance(&request, 1_299),
            Err(Refusal::CriticalNotCleared {
                op: op.as_str().to_owned()
            })
        );
        assert_eq!(
            delay.clearance(&request, 1_300),
            Ok(Clearance::Delay { ready_at: 1_300 })
        );
        assert_eq!(
            NoCritical.clearance(&request, 9_999),
            Err(Refusal::CriticalNotCleared {
                op: op.as_str().to_owned()
            })
        );
    }

    #[test]
    fn default_policy_is_strict() {
        let policy = Policy::default();
        assert!(policy.requester_cannot_approve);
        assert!(policy.require_hardware_keys);
        assert_eq!(policy.verify_policy(2).min_signatures, 2);
    }
}
