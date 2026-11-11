//! The plan digest: what an approver approves and what apply must run.

use sha2::{Digest, Sha256};
use spot_migration_plan::ReleasePlan;

/// Domain separation prefix; the digest of a plan can never collide with any other SHA-256
/// the platform computes.
pub const DOMAIN_PREFIX: &[u8] = b"spotledger-migration-plan-v1\0";

/// SHA-256 (lowercase hex) over [`DOMAIN_PREFIX`] and the plan's canonical text
/// (`ReleasePlan::json`, which includes every change, its risk class, the estimate and the
/// target description).
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct PlanDigest(String);

impl PlanDigest {
    /// The digest of a plan, recomputed from its content (the plan's own `plan_digest` field
    /// is part of what is hashed, never trusted on its own).
    #[must_use]
    pub fn of(plan: &ReleasePlan) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_PREFIX);
        hasher.update(plan.json().as_bytes());
        let bytes = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in bytes {
            out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        Self(out)
    }

    /// The lowercase hex form.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
