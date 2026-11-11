//! The rehearsal report: structured (JSON) and rendered (text).

use serde::{Deserialize, Serialize};

use crate::applier::ApplyOutcome;
use crate::clone::CleanupOutcome;
use crate::fingerprint::{Dimension, FingerprintDiff};
use crate::invariants::InvariantReport;
use crate::replay::ReplayDecision;

/// How a fingerprint difference was classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffClass {
    /// A plan change declares it moves this section.
    Expected,
    /// No plan change explains it.
    Unexplained,
}

/// One fingerprint difference with its classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedDiff {
    /// The section key.
    pub section: String,
    /// The dimensions of the affected row.
    pub dimensions: Vec<Dimension>,
    /// The measure name.
    pub measure: String,
    /// The value before the plan was applied (a decimal string on the wire).
    #[serde(with = "crate::wire")]
    pub before: i128,
    /// The value after the plan was applied (a decimal string on the wire).
    #[serde(with = "crate::wire")]
    pub after: i128,
    /// `after - before` (a decimal string on the wire).
    #[serde(with = "crate::wire")]
    pub delta: i128,
    /// Whether the plan explains it.
    pub class: DiffClass,
    /// The plan change id that explains it, when the class is expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explained_by: Option<String>,
}

impl ClassifiedDiff {
    /// A difference classified as expected by `change`.
    pub fn expected(diff: &FingerprintDiff, change: impl Into<String>) -> ClassifiedDiff {
        ClassifiedDiff {
            section: diff.section.clone(),
            dimensions: diff.dimensions.clone(),
            measure: diff.measure.clone(),
            before: diff.before,
            after: diff.after,
            delta: diff.delta(),
            class: DiffClass::Expected,
            explained_by: Some(change.into()),
        }
    }

    /// A difference the plan does not explain.
    pub fn unexplained(diff: &FingerprintDiff) -> ClassifiedDiff {
        ClassifiedDiff {
            section: diff.section.clone(),
            dimensions: diff.dimensions.clone(),
            measure: diff.measure.clone(),
            before: diff.before,
            after: diff.after,
            delta: diff.delta(),
            class: DiffClass::Unexplained,
            explained_by: None,
        }
    }
}

/// The clone the rehearsal ran on and how it was disposed of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneRecord {
    /// The site.
    pub site: String,
    /// The scratch's label (a database name or a data-directory path). Never a secret.
    pub label: String,
    /// The snapshot the clone was restored from.
    pub snapshot_id: String,
    /// How the clone was disposed of.
    pub cleanup: CleanupOutcome,
}

/// The verdict of a rehearsal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalOutcome {
    /// No fingerprint difference and every invariant held.
    Clean,
    /// Differences, all of them declared by the plan, and every invariant held.
    ExpectedChanges,
    /// At least one difference the plan does not explain.
    UnexplainedDifferences,
    /// At least one invariant set failed.
    InvariantsFailed,
}

/// Everything one rehearsal produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalReport {
    /// The approved ticket id.
    pub ticket: String,
    /// The site.
    pub site: String,
    /// The digest of the plan that was rehearsed.
    pub plan_digest: String,
    /// The clone and its cleanup.
    pub clone: CloneRecord,
    /// The digest of the pre fingerprint.
    pub pre_digest: String,
    /// The digest of the post fingerprint.
    pub post_digest: String,
    /// The invariant sets' outcome.
    pub invariants: InvariantReport,
    /// What the applier did.
    pub apply: ApplyOutcome,
    /// Every fingerprint difference, classified.
    pub diffs: Vec<ClassifiedDiff>,
    /// The replay decision.
    pub replay: ReplayDecision,
    /// The verdict.
    pub outcome: RehearsalOutcome,
}

impl RehearsalReport {
    /// The unexplained differences.
    pub fn unexplained(&self) -> impl Iterator<Item = &ClassifiedDiff> {
        self.diffs
            .iter()
            .filter(|d| d.class == DiffClass::Unexplained)
    }

    /// The differences the plan explains.
    pub fn expected(&self) -> impl Iterator<Item = &ClassifiedDiff> {
        self.diffs.iter().filter(|d| d.class == DiffClass::Expected)
    }

    /// The verdict for the given differences and invariant outcome.
    pub fn outcome_of(diffs: &[ClassifiedDiff], invariants: &InvariantReport) -> RehearsalOutcome {
        if !invariants.passed() {
            return RehearsalOutcome::InvariantsFailed;
        }
        if diffs.iter().any(|d| d.class == DiffClass::Unexplained) {
            return RehearsalOutcome::UnexplainedDifferences;
        }
        if diffs.is_empty() {
            RehearsalOutcome::Clean
        } else {
            RehearsalOutcome::ExpectedChanges
        }
    }

    /// The report as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// The report as text for a terminal or a review comment.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Rehearsal of ticket {} on site {}\n",
            self.ticket, self.site
        ));
        out.push_str(&format!("  plan digest : {}\n", self.plan_digest));
        out.push_str(&format!(
            "  clone       : {} (snapshot {})\n",
            self.clone.label, self.clone.snapshot_id
        ));
        out.push_str(&format!(
            "  fingerprint : {} -> {}\n",
            short(&self.pre_digest),
            short(&self.post_digest)
        ));
        out.push_str(&format!(
            "  replay      : {} ({})\n",
            if self.replay.replay { "on" } else { "off" },
            self.replay.reason
        ));
        let invariants = if self.invariants.passed() {
            "passed".to_owned()
        } else {
            format!(
                "FAILED: {} ({})",
                self.invariants.failed_sets().join(", "),
                self.invariants.total()
            )
        };
        out.push_str(&format!("  invariants  : {invariants}\n"));
        if self.diffs.is_empty() {
            out.push_str("  fingerprint diff: empty\n");
        } else {
            out.push_str(&format!(
                "  fingerprint diff: {} difference(s)\n",
                self.diffs.len()
            ));
            for diff in &self.diffs {
                let where_ = diff
                    .dimensions
                    .iter()
                    .map(|d| format!("{}={}", d.name, d.value))
                    .collect::<Vec<_>>()
                    .join(",");
                let class = match diff.class {
                    DiffClass::Expected => {
                        format!("expected ({})", diff.explained_by.as_deref().unwrap_or("?"))
                    }
                    DiffClass::Unexplained => "UNEXPLAINED".to_owned(),
                };
                out.push_str(&format!(
                    "    [{}] {} {where_} {} {} -> {} ({})\n",
                    class, diff.section, diff.measure, diff.before, diff.after, diff.delta
                ));
            }
        }
        out.push_str(&format!("  outcome     : {}\n", outcome_name(self.outcome)));
        match &self.clone.cleanup {
            CleanupOutcome::Cleaned => {}
            CleanupOutcome::Failed { reason } => {
                out.push_str(&format!("  cleanup     : FAILED: {reason}\n"))
            }
        }
        out
    }
}

/// The outcome's name.
fn outcome_name(outcome: RehearsalOutcome) -> &'static str {
    match outcome {
        RehearsalOutcome::Clean => "clean",
        RehearsalOutcome::ExpectedChanges => "expected changes only",
        RehearsalOutcome::UnexplainedDifferences => "UNEXPLAINED differences",
        RehearsalOutcome::InvariantsFailed => "invariants failed",
    }
}

/// The first twelve hex characters of a digest.
fn short(digest: &str) -> &str {
    &digest[..digest.len().min(12)]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::invariants::{InvariantReport, InvariantSetResult, InvariantViolation};
    use crate::replay::ReplayPolicy;

    fn diff(section: &str, measure: &str, before: i128, after: i128) -> FingerprintDiff {
        FingerprintDiff {
            section: section.to_owned(),
            dimensions: vec![Dimension::new("account", "cash")],
            measure: measure.to_owned(),
            before,
            after,
        }
    }

    fn empty_report(diffs: Vec<ClassifiedDiff>, invariants: InvariantReport) -> RehearsalReport {
        let outcome = RehearsalReport::outcome_of(&diffs, &invariants);
        RehearsalReport {
            ticket: "CHG-1".to_owned(),
            site: "localhost".to_owned(),
            plan_digest: "abc".to_owned(),
            clone: CloneRecord {
                site: "localhost".to_owned(),
                label: "spot_scratch".to_owned(),
                snapshot_id: "snap-1".to_owned(),
                cleanup: CleanupOutcome::Cleaned,
            },
            pre_digest: "a".repeat(64),
            post_digest: "b".repeat(64),
            invariants,
            apply: crate::applier::ApplyOutcome {
                executed: Vec::new(),
                plan_hash: String::new(),
                plan_digest: "abc".to_owned(),
                backfilled_rows: 0,
            },
            diffs,
            replay: ReplayDecision {
                policy: ReplayPolicy::Off,
                replay: false,
                reason: "off".to_owned(),
                triggers: Vec::new(),
            },
            outcome,
        }
    }

    #[test]
    fn no_differences_and_no_violations_is_clean() {
        let report = empty_report(Vec::new(), InvariantReport::default());
        assert_eq!(report.outcome, RehearsalOutcome::Clean);
        assert!(report.render_text().contains("diff: empty"));
    }

    #[test]
    fn a_failed_invariant_wins_over_a_clean_diff() {
        let invariants = InvariantReport {
            sets: vec![InvariantSetResult {
                set: "framework".to_owned(),
                passed: false,
                violations: vec![InvariantViolation::new(
                    "framework",
                    "seq-gapless",
                    "sequence",
                    "-",
                    "expected=3 actual=2",
                )],
            }],
        };
        let report = empty_report(Vec::new(), invariants);
        assert_eq!(report.outcome, RehearsalOutcome::InvariantsFailed);
        assert!(report.render_text().contains("FAILED"));
    }

    #[test]
    fn expected_and_unexplained_are_separated() {
        let diffs = vec![
            ClassifiedDiff::expected(&diff("trial_balance", "debit", 10, 11), "behaviour/x"),
            ClassifiedDiff::unexplained(&diff("stock_balance", "value", 5, 6)),
        ];
        let report = empty_report(diffs, InvariantReport::default());
        assert_eq!(report.outcome, RehearsalOutcome::UnexplainedDifferences);
        assert_eq!(report.unexplained().count(), 1);
        assert_eq!(report.expected().count(), 1);
        assert!(report.render_text().contains("UNEXPLAINED"));
    }
}
