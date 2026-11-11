//! The fault report: one JSON document per scenario, with the fault timeline.
//!
//! The CI script ([`crate::scenarios`]'s `ci/run.sh`) writes the report and the timeline as
//! artefacts and exits non-zero on any violation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::audit::CommitClass;
use crate::faults::FaultEvent;

/// The `schema` value of a fault report.
pub const FAULT_REPORT_SCHEMA: &str = "spot-l8-faults-report/1";

/// Everything one scenario found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultReport {
    /// Always [`FAULT_REPORT_SCHEMA`].
    pub schema: String,
    /// The scenario name.
    pub scenario: String,
    /// The seed: pass it again to replay the schedule.
    pub seed: u64,
    /// The profile name.
    pub profile: String,
    /// Whether every check held.
    pub ok: bool,
    /// Crash-recovery time, when the scenario killed the server.
    pub recovery_ms: Option<u64>,
    /// The durability audit's classification table.
    pub classification: BTreeMap<CommitClass, u64>,
    /// Whether the L7 invariants held.
    pub l7_ok: bool,
    /// Whether the durability audit held.
    pub audit_ok: bool,
    /// Whether the isolation checker held, when it ran.
    pub isolation_ok: Option<bool>,
    /// Anything the scenario could not do or chose to skip.
    pub notes: Vec<String>,
    /// Every violation found, formatted `invariant entity id detail`.
    pub violations: Vec<String>,
    /// The faults that fired, with times.
    pub fault_timeline: Vec<FaultEvent>,
}

impl FaultReport {
    /// Pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// Plain text for a terminal or a CI log.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "fault scenario {}: {} (profile {}, seed {})\n",
            self.scenario,
            if self.ok { "OK" } else { "VIOLATIONS" },
            self.profile,
            self.seed
        ));
        if let Some(ms) = self.recovery_ms {
            out.push_str(&format!("  crash recovery: {ms} ms\n"));
        }
        out.push_str(&format!(
            "  l7 invariants: {}, durability audit: {}, isolation: {}\n",
            pass(self.l7_ok),
            pass(self.audit_ok),
            self.isolation_ok.map(pass).unwrap_or("not run")
        ));
        for class in CommitClass::all() {
            let count = self.classification.get(&class).copied().unwrap_or(0);
            out.push_str(&format!("  {:<24} {count:>8}\n", class.name()));
        }
        for note in &self.notes {
            out.push_str(&format!("  note: {note}\n"));
        }
        for violation in &self.violations {
            out.push_str(&format!("  VIOLATION: {violation}\n"));
        }
        if !self.fault_timeline.is_empty() {
            out.push_str("  fault timeline:\n");
            for e in &self.fault_timeline {
                out.push_str(&format!(
                    "    {:>10}us conn={} dir={} {} {}\n",
                    e.at_us,
                    e.conn,
                    e.direction.name(),
                    e.fault.name(),
                    e.detail
                ));
            }
        }
        out
    }
}

fn pass(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}
