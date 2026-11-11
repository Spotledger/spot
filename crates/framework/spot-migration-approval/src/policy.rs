//! The per-site approval policy, by risk class. Stored in settings as a JSON value under
//! [`SETTING_KEY`].

use serde::{Deserialize, Serialize};
use spot_migration_plan::Risk;

use crate::error::ApprovalError;

/// The settings key the policy is stored under.
pub const SETTING_KEY: &str = concat!("migration", ".approval_policy");

/// One hour in microseconds.
const HOUR_MICROS: i64 = 3_600_000_000;

/// One day in microseconds.
const DAY_MICROS: i64 = 86_400_000_000;

/// Rule for additive plans.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdditiveRule {
    /// When true, an additive plan needs no approval.
    pub auto_approve: bool,
    /// Distinct approvers needed when `auto_approve` is false.
    pub approvers: u32,
    /// The role an approver must hold, if any.
    pub role: Option<String>,
}

/// Rule for needs-review plans.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRule {
    /// Distinct approvers needed (at least 1).
    pub approvers: u32,
    /// The role an approver must hold, if any.
    pub role: Option<String>,
}

/// Rule for destructive plans.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestructiveRule {
    /// Distinct approvers needed (at least 2: two-person).
    pub approvers: u32,
    /// The role an approver must hold, if any.
    pub role: Option<String>,
    /// Each approval must carry a signature the hardware hook verifies.
    pub require_hardware_signature: bool,
}

/// The approval policy of one site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    /// Additive plans.
    pub additive: AdditiveRule,
    /// Needs-review plans.
    pub needs_review: ReviewRule,
    /// Destructive plans.
    pub destructive: DestructiveRule,
    /// How long an approval stays valid, in microseconds (at least 1).
    pub expiry_micros: i64,
    /// An approver may not approve a plan they authored.
    pub forbid_author_approval: bool,
}

/// `Default` is the loose starting point for tests and explicit configuration (additive plans
/// auto-approve). **No path that reads settings may use it for "unset"**: an absent policy is
/// [`ApprovalPolicy::strictest`].
impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            additive: AdditiveRule {
                auto_approve: true,
                approvers: 1,
                role: None,
            },
            needs_review: ReviewRule {
                approvers: 1,
                role: Some("release_manager".to_owned()),
            },
            destructive: DestructiveRule {
                approvers: 2,
                role: Some("release_manager".to_owned()),
                require_hardware_signature: false,
            },
            expiry_micros: DAY_MICROS,
            forbid_author_approval: true,
        }
    }
}

impl ApprovalPolicy {
    /// The strictest valid policy: every plan, additive ones too, needs two approvers in the
    /// `release_manager` role; destructive plans also need a hardware signature; approvals last
    /// an hour; authors may not approve. This is what an absent, unreadable or invalid policy
    /// setting means; a site that wants looser rules sets the policy explicitly.
    #[must_use]
    pub fn strictest() -> Self {
        Self {
            additive: AdditiveRule {
                auto_approve: false,
                approvers: 2,
                role: Some("release_manager".to_owned()),
            },
            needs_review: ReviewRule {
                approvers: 2,
                role: Some("release_manager".to_owned()),
            },
            destructive: DestructiveRule {
                approvers: 2,
                role: Some("release_manager".to_owned()),
                require_hardware_signature: true,
            },
            expiry_micros: HOUR_MICROS,
            forbid_author_approval: true,
        }
    }
}

/// What the policy asks of one plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// Distinct valid approvers needed (0 means auto-approved).
    pub approvers: u32,
    /// The role each must hold.
    pub role: Option<String>,
    /// Whether each approval needs a verified hardware signature.
    pub hardware_signature: bool,
}

impl ApprovalPolicy {
    /// Checks the policy is usable.
    ///
    /// # Errors
    /// [`ApprovalError::BadPolicy`] when a destructive plan could be approved by fewer than
    /// two people, a review by nobody, or the expiry is not positive.
    pub fn validate(&self) -> Result<(), ApprovalError> {
        let bad = |message: &str| {
            Err(ApprovalError::BadPolicy {
                message: message.to_owned(),
            })
        };
        if self.destructive.approvers < 2 {
            return bad("a destructive plan needs at least two approvers");
        }
        if self.needs_review.approvers < 1 {
            return bad("a needs-review plan needs at least one approver");
        }
        if !self.additive.auto_approve && self.additive.approvers < 1 {
            return bad("an additive plan that is not auto-approved needs an approver");
        }
        if self.expiry_micros < 1 {
            return bad("the approval expiry must be positive");
        }
        Ok(())
    }

    /// What this policy requires for a plan of `risk`.
    #[must_use]
    pub fn requirement(&self, risk: Risk) -> Requirement {
        match risk {
            Risk::Additive if self.additive.auto_approve => Requirement {
                approvers: 0,
                role: None,
                hardware_signature: false,
            },
            Risk::Additive => Requirement {
                approvers: self.additive.approvers,
                role: self.additive.role.clone(),
                hardware_signature: false,
            },
            Risk::NeedsReview => Requirement {
                approvers: self.needs_review.approvers,
                role: self.needs_review.role.clone(),
                hardware_signature: false,
            },
            Risk::Destructive => Requirement {
                approvers: self.destructive.approvers,
                role: self.destructive.role.clone(),
                hardware_signature: self.destructive.require_hardware_signature,
            },
        }
    }

    /// The settings value (JSON).
    #[must_use]
    pub fn to_setting(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Reads and validates the settings value.
    ///
    /// # Errors
    /// [`ApprovalError::BadPolicy`] when it does not parse or fails [`Self::validate`].
    pub fn from_setting(text: &str) -> Result<Self, ApprovalError> {
        let policy: Self = serde_json::from_str(text).map_err(|e| ApprovalError::BadPolicy {
            message: format!("the policy setting does not parse: {e}"),
        })?;
        policy.validate()?;
        Ok(policy)
    }
}
