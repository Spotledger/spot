//! The support tiers: the one ladder the agent authoriser defines, plus this crate's wire form.
//!
//! The platform's tier ladder (`T0` Explain, `T1` Configure, `T2` Repair data, `T3`
//! Structural) is defined once, by the agent authoriser's crate (WP-AI-06.T1, blueprint
//! WS13). This crate used to keep a local mirror; it now re-exports that type, so a tier added
//! there reaches every session check at once (WP-AI-06.T3).
//!
//! **Wire form.** Stored and transmitted session grants write a tier as `t0` to `t3`
//! (snake_case) while agent records write `T0` to `T3`. Every tier field of this crate's own
//! types uses [`tier_snake`] through `#[serde(with = ...)]`: it writes `t0` to `t3`, so a grant
//! written today is byte-identical to one written before the unification, and it reads either
//! case, so a record written by the agent side also loads.

pub use spot_agent_tiers::Tier;

/// `#[serde(with = "...")]` helper that writes a [`Tier`] as `t0` to `t3` and reads `t0` to `t3`
/// or `T0` to `T3`. Anything else is an error; there is no default tier.
pub mod tier_snake {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Tier;

    /// The wire text of a tier (`t0` to `t3`).
    #[must_use]
    pub fn wire(tier: Tier) -> &'static str {
        match tier {
            Tier::T0 => "t0",
            Tier::T1 => "t1",
            Tier::T2 => "t2",
            Tier::T3 => "t3",
        }
    }

    /// Writes the tier as `t0` to `t3`.
    pub fn serialize<S: Serializer>(tier: &Tier, serializer: S) -> Result<S::Ok, S::Error> {
        wire(*tier).serialize(serializer)
    }

    /// Reads a tier written in either case.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Tier, D::Error> {
        // The shared type accepts `T0`..`T3` and `t0`..`t3` (and nothing else).
        Tier::deserialize(deserializer)
    }
}
