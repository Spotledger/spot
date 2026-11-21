//! The T0 to T3 tier ladder (blueprint WS13): what each tier may do and when it may run unattended.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How much an action may change, and who must be around when it runs.
///
/// Ordered: `T0 < T1 < T2 < T3`. Written as `T0`..`T3`; on read the lower-case form `t0`..`t3`
/// (the support-session wire form before the two tier types were unified) is accepted too. The tiers follow the blueprint's WS13 table: T0 Explain
/// (read and diagnose any issue), T1 Configure (reversible settings: permissions within
/// policy, print formats, cache, retry or unstick jobs), T2 Repair data (remedies from the
/// catalogue only: repost, recompute, relink, unstick workflow state), T3 Structural
/// (customizations, scripts, app installs, migrations).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// T0 Explain: read and diagnose any issue. May run unattended.
    #[serde(rename = "T0", alias = "t0")]
    T0,
    /// T1 Configure: reversible settings within policy. May run unattended.
    #[serde(rename = "T1", alias = "t1")]
    T1,
    /// T2 Repair data: catalogued remedies only. May run unattended.
    #[serde(rename = "T2", alias = "t2")]
    T2,
    /// T3 Structural: customizations, scripts, app installs, migrations. Runs unattended
    /// only for a registered deep agent; otherwise the request needs a human approval.
    #[serde(rename = "T3", alias = "t3")]
    T3,
}

impl Tier {
    /// Whether an agent of this tier may act unattended.
    ///
    /// T0 to T2 always may; T3 only when the agent is registered deep — otherwise the
    /// request must carry a human approval, and the authoriser checks it.
    #[must_use]
    pub fn unattended_allowed(self, deep: bool) -> bool {
        match self {
            Tier::T0 | Tier::T1 | Tier::T2 => true,
            Tier::T3 => deep,
        }
    }

    /// Every tier, weakest first, in canonical (enum) order.
    #[must_use]
    pub fn all() -> [Tier; 4] {
        [Tier::T0, Tier::T1, Tier::T2, Tier::T3]
    }

    /// Whether the tier only reads and diagnoses (`T0`); every higher tier changes
    /// something.
    #[must_use]
    pub fn is_read_only(self) -> bool {
        matches!(self, Tier::T0)
    }

    /// The stable text form (`T0` to `T3`), which is also the serde and TOML form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
            Tier::T3 => "T3",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Tier;

    #[test]
    fn the_ladder_is_ordered_and_all_lists_it_weakest_first() {
        assert!(Tier::T0 < Tier::T1 && Tier::T1 < Tier::T2 && Tier::T2 < Tier::T3);
        let all = Tier::all();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn only_t0_is_read_only() {
        let read_only: Vec<Tier> = Tier::all()
            .into_iter()
            .filter(|t| t.is_read_only())
            .collect();
        assert_eq!(read_only, [Tier::T0]);
    }

    #[test]
    fn display_and_as_str_agree_and_unattended_follows_the_deep_flag() {
        for t in Tier::all() {
            assert_eq!(t.to_string(), t.as_str());
            assert!(t.unattended_allowed(true));
            assert_eq!(t.unattended_allowed(false), t != Tier::T3);
        }
    }

    #[test]
    fn serde_writes_upper_case_and_reads_either_case() {
        for t in Tier::all() {
            let upper = format!("\"{}\"", t.as_str());
            let lower = format!("\"{}\"", t.as_str().to_lowercase());
            assert_eq!(serde_json::to_string(&t).unwrap_or_default(), upper);
            assert_eq!(serde_json::from_str::<Tier>(&upper).ok(), Some(t));
            assert_eq!(serde_json::from_str::<Tier>(&lower).ok(), Some(t));
        }
        assert!(serde_json::from_str::<Tier>("\"T4\"").is_err());
        assert!(serde_json::from_str::<Tier>("\"t\"").is_err());
    }
}
