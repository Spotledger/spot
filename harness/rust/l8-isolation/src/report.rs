//! The isolation report: what a run claimed, what the checker observed, and the verdict.

use serde::{Deserialize, Serialize};
use spot_l7_invariants::history::Level;

use crate::anomaly::{AnomalyClass, Witness};
use crate::history::Workload;
use crate::levels::level_describe;

/// The `schema` value of a report.
pub const REPORT_SCHEMA: &str = "spot-l8-report/1";

/// The outcome of checking a history against what it claimed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// No anomaly the claimed level forbids was found. (Anomalies the level *allows* may still
    /// have been observed; they are listed under `observed_allowed`.)
    Consistent,
    /// The claimed level was violated.
    Violation {
        /// The level claimed.
        claimed: Level,
        /// The classes observed that the level forbids.
        observed: Vec<AnomalyClass>,
    },
}

/// What one anomaly class contributed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassReport {
    /// The class.
    pub class: AnomalyClass,
    /// Whether the claimed level forbids it.
    pub forbidden: bool,
    /// How many were found (the witnesses below are capped).
    pub total: u64,
    /// The witnesses, at most
    /// [`MAX_WITNESSES_PER_CLASS`](crate::checker::MAX_WITNESSES_PER_CLASS).
    pub witnesses: Vec<Witness>,
}

/// Everything a check found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationReport {
    /// Always [`REPORT_SCHEMA`].
    pub schema: String,
    /// The claimed level.
    pub claimed: Level,
    /// The workload.
    pub workload: Workload,
    /// The run's seed.
    pub seed: u64,
    /// Workers.
    pub workers: u32,
    /// Transactions in the history.
    pub transactions: u64,
    /// Transactions that committed.
    pub committed: u64,
    /// Transactions known to have aborted.
    pub aborted: u64,
    /// Transactions whose outcome is unknown.
    pub unknown: u64,
    /// The verdict.
    pub verdict: Verdict,
    /// Classes observed that the claimed level forbids.
    pub violations: Vec<AnomalyClass>,
    /// Classes observed that the claimed level allows.
    pub observed_allowed: Vec<AnomalyClass>,
    /// The strongest level the whole history is consistent with, if any.
    pub strongest_consistent: Option<Level>,
    /// Per-class detail, in [`AnomalyClass::all`] order.
    pub classes: Vec<ClassReport>,
}

impl IsolationReport {
    /// Whether the claimed level held.
    pub fn ok(&self) -> bool {
        matches!(self.verdict, Verdict::Consistent)
    }

    /// How many anomalies of a class were found.
    pub fn count(&self, class: AnomalyClass) -> u64 {
        self.classes
            .iter()
            .find(|c| c.class == class)
            .map_or(0, |c| c.total)
    }

    /// The witnesses of a class.
    pub fn witnesses_of(&self, class: AnomalyClass) -> &[Witness] {
        self.classes
            .iter()
            .find(|c| c.class == class)
            .map_or(&[], |c| c.witnesses.as_slice())
    }

    /// The classes observed at all.
    pub fn observed(&self) -> Vec<AnomalyClass> {
        self.classes.iter().map(|c| c.class).collect()
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// Plain text for a terminal or a CI log.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "L8 isolation: {} — claimed {} (workload {}, seed {}, {} workers)\n",
            if self.ok() { "OK" } else { "VIOLATION" },
            level_describe(self.claimed),
            self.workload.name(),
            self.seed,
            self.workers,
        ));
        out.push_str(&format!(
            "transactions: {} total, {} committed, {} aborted, {} unknown\n",
            self.transactions, self.committed, self.aborted, self.unknown
        ));
        match &self.strongest_consistent {
            Some(l) => out.push_str(&format!(
                "strongest level consistent with the history: {}\n",
                level_describe(*l)
            )),
            None => out.push_str("strongest level consistent with the history: weaker than rc\n"),
        }
        if self.violations.is_empty() {
            out.push_str("no forbidden anomaly observed\n");
        } else {
            let names: Vec<&str> = self.violations.iter().map(|c| c.name()).collect();
            out.push_str(&format!(
                "forbidden anomalies observed: {}\n",
                names.join(", ")
            ));
        }
        if !self.observed_allowed.is_empty() {
            let names: Vec<&str> = self.observed_allowed.iter().map(|c| c.name()).collect();
            out.push_str(&format!(
                "allowed anomalies observed: {}\n",
                names.join(", ")
            ));
        }
        for c in &self.classes {
            out.push_str(&format!(
                "  [{}] {} ({}) x{}\n",
                if c.forbidden { "FAIL" } else { "allow" },
                c.class.name(),
                c.class.describe(),
                c.total
            ));
            for w in &c.witnesses {
                out.push_str(&format!("      {}\n", w.story));
            }
            if c.total as usize > c.witnesses.len() {
                out.push_str(&format!(
                    "      ... and {} more\n",
                    c.total as usize - c.witnesses.len()
                ));
            }
        }
        out
    }
}
