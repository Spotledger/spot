//! The compliance report: one outcome per control, a score, a structured JSON form, a plain
//! text summary, and a diff between two reports.
//!
//! The report is deterministic: the results are in the baseline's declaration order, the score
//! is integer arithmetic (no floats), and the JSON is `serde_json`'s pretty form of the same
//! structs. The diff pairs two reports by control id in id order and reports only the controls
//! whose status changed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::HardeningError;
use crate::model::{ControlId, OpRef, OsFamily, Severity};

/// The one-word status of a control, the coarse form the diff and the score use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The control passed.
    Pass,
    /// The control failed.
    Fail,
    /// The control does not apply to this host.
    NotApplicable,
    /// The control could not be evaluated.
    Error,
}

/// What one control's evaluation found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    /// The control passed.
    Pass,
    /// The control failed; `found` is what the host reports and `expected` is what the baseline
    /// requires, both in words so the failure is explainable.
    Fail {
        /// What the host reports.
        found: String,
        /// What the baseline requires.
        expected: String,
    },
    /// The control does not apply (a fact is not reported on this host).
    NotApplicable {
        /// Why the control does not apply.
        reason: String,
    },
    /// The control could not be evaluated (a fact source failed).
    Error {
        /// What went wrong, in words.
        detail: String,
    },
}

impl Outcome {
    /// The coarse status of this outcome.
    #[must_use]
    pub fn status(&self) -> Status {
        match self {
            Self::Pass => Status::Pass,
            Self::Fail { .. } => Status::Fail,
            Self::NotApplicable { .. } => Status::NotApplicable,
            Self::Error { .. } => Status::Error,
        }
    }

    /// Whether this outcome is a failure (the only outcome a remedy addresses).
    #[must_use]
    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }
}

/// The result of evaluating one control, with everything the planner needs to act on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResult {
    /// The control's stable id.
    pub id: ControlId,
    /// The control's human title.
    pub title: String,
    /// The control's severity.
    pub severity: Severity,
    /// What the evaluation found.
    pub outcome: Outcome,
    /// The operation that remediates a failure, when one exists.
    #[serde(default)]
    pub remedy: Option<OpRef>,
    /// Whether the remedy is reversible.
    #[serde(default)]
    pub reversible: bool,
    /// Whether applying the remedy can cut off access to the host.
    #[serde(default)]
    pub lockout_risk: bool,
}

impl ControlResult {
    /// Whether the control carries a reversible remedy — whether the agent *can* fix it,
    /// regardless of whether it currently fails.
    #[must_use]
    pub fn auto_remediable(&self) -> bool {
        self.remedy.is_some() && self.reversible
    }
}

/// The report's score, as whole-number counts plus a basis-point figure.
///
/// `basis_points` is `passed * 10000 / applicable`, where `applicable` is
/// `passed + failed + errored` (not-applicable controls are excluded because they do not apply
/// to the host). With no applicable controls the score is `10000`. Integer arithmetic only, so
/// the score is exact and deterministic (no floats, ADR-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    /// Controls that passed.
    pub passed: usize,
    /// Controls that failed.
    pub failed: usize,
    /// Controls that do not apply to this host.
    pub not_applicable: usize,
    /// Controls that could not be evaluated.
    pub errored: usize,
    /// `passed + failed + errored`: the controls the score is computed over.
    pub applicable: usize,
    /// The pass ratio in basis points (0 to 10000).
    pub basis_points: u32,
}

impl Score {
    /// Scores a slice of results.
    #[must_use]
    pub fn of(results: &[ControlResult]) -> Self {
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut not_applicable = 0usize;
        let mut errored = 0usize;
        for result in results {
            match result.outcome.status() {
                Status::Pass => passed += 1,
                Status::Fail => failed += 1,
                Status::NotApplicable => not_applicable += 1,
                Status::Error => errored += 1,
            }
        }
        let applicable = passed + failed + errored;
        let basis_points = passed
            .checked_mul(10_000)
            .and_then(|scaled| scaled.checked_div(applicable))
            .map_or(10_000, |value| value as u32);
        Self {
            passed,
            failed,
            not_applicable,
            errored,
            applicable,
            basis_points,
        }
    }
}

/// A compliance report: the OS family, one result per control and the score.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The OS family the baseline applies to.
    pub os_family: OsFamily,
    /// One result per control, in the baseline's declaration order.
    pub results: Vec<ControlResult>,
    /// The score over the results.
    pub score: Score,
}

impl Report {
    /// The result for `id`, when the report has one.
    #[must_use]
    pub fn result(&self, id: &ControlId) -> Option<&ControlResult> {
        self.results.iter().find(|result| &result.id == id)
    }

    /// The report as pretty-printed JSON.
    ///
    /// # Errors
    ///
    /// [`HardeningError::ReportInvalid`] when serialization fails (it does not for these
    /// types, but the error is surfaced rather than hidden).
    pub fn to_json(&self) -> Result<String, HardeningError> {
        serde_json::to_string_pretty(self).map_err(|error| HardeningError::ReportInvalid {
            detail: error.to_string(),
        })
    }

    /// Reads a report back from its JSON form.
    ///
    /// # Errors
    ///
    /// [`HardeningError::ReportInvalid`] when the text is not a valid report.
    pub fn from_json(text: &str) -> Result<Self, HardeningError> {
        serde_json::from_str(text).map_err(|error| HardeningError::ReportInvalid {
            detail: error.to_string(),
        })
    }

    /// A plain text summary, one line per control, no HTML and no markup.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Hardening compliance: {} — {} of {} applicable controls pass ({} basis points); \
             {} not applicable, {} errored\n",
            self.os_family.as_str(),
            self.score.passed,
            self.score.applicable,
            self.score.basis_points,
            self.score.not_applicable,
            self.score.errored
        ));
        for result in &self.results {
            let (label, detail) = match &result.outcome {
                Outcome::Pass => ("PASS ", String::new()),
                Outcome::Fail { found, expected } => {
                    ("FAIL ", format!("expected {expected}, found {found}"))
                }
                Outcome::NotApplicable { reason } => ("N/A  ", reason.clone()),
                Outcome::Error { detail } => ("ERROR", detail.clone()),
            };
            out.push_str(&format!(
                "{label} {:<32} {:<8} {}\n",
                result.id.as_str(),
                severity_name(result.severity),
                detail
            ));
        }
        out
    }
}

/// A severity's lower-case name, as the report spells it.
fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
    }
}

/// The direction of one control's change between two reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The control is only in the later report.
    Added,
    /// The control is only in the earlier report.
    Removed,
    /// The control went from failing to passing.
    Improved,
    /// The control went from passing to failing.
    Regressed,
    /// The status changed in any other way (for example a failure became an error).
    Changed,
}

/// One control's change between two reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlChange {
    /// The control's id.
    pub id: ControlId,
    /// The status in the earlier report, when present.
    pub before: Option<Status>,
    /// The status in the later report, when present.
    pub after: Option<Status>,
    /// The direction of the change.
    pub direction: Direction,
}

/// The difference between two reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportDiff {
    /// The OS family of the later report.
    pub os_family: OsFamily,
    /// The changed controls, in id order.
    pub changes: Vec<ControlChange>,
}

impl ReportDiff {
    /// Whether the two reports are identical in every control's status.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// The difference between two reports, pairing controls by id in id order and keeping only
/// the ones whose status changed.
#[must_use]
pub fn diff(before: &Report, after: &Report) -> ReportDiff {
    let before_by_id: BTreeMap<&ControlId, Status> = before
        .results
        .iter()
        .map(|result| (&result.id, result.outcome.status()))
        .collect();
    let after_by_id: BTreeMap<&ControlId, Status> = after
        .results
        .iter()
        .map(|result| (&result.id, result.outcome.status()))
        .collect();

    let mut ids: Vec<&ControlId> = before_by_id
        .keys()
        .chain(after_by_id.keys())
        .copied()
        .collect();
    ids.sort();
    ids.dedup();

    let mut changes = Vec::new();
    for id in ids {
        let old = before_by_id.get(id).copied();
        let new = after_by_id.get(id).copied();
        if old == new {
            continue;
        }
        let direction = match (old, new) {
            (None, Some(_)) => Direction::Added,
            (Some(_), None) => Direction::Removed,
            (Some(Status::Fail), Some(Status::Pass)) => Direction::Improved,
            (Some(Status::Pass), Some(Status::Fail)) => Direction::Regressed,
            _ => Direction::Changed,
        };
        changes.push(ControlChange {
            id: (*id).clone(),
            before: old,
            after: new,
            direction,
        });
    }
    ReportDiff {
        os_family: after.os_family,
        changes,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::{FactKey, OsFamily};

    fn result(id: &str, outcome: Outcome) -> ControlResult {
        ControlResult {
            id: ControlId::try_new(id).unwrap(),
            title: "a control".to_owned(),
            severity: Severity::High,
            outcome,
            remedy: None,
            reversible: false,
            lockout_risk: false,
        }
    }

    fn report(results: Vec<ControlResult>) -> Report {
        let score = Score::of(&results);
        Report {
            os_family: OsFamily::Linux,
            results,
            score,
        }
    }

    #[test]
    fn score_excludes_not_applicable_and_uses_basis_points() {
        let report = report(vec![
            result("a.one", Outcome::Pass),
            result("a.two", Outcome::Pass),
            result("a.three", Outcome::Pass),
            result(
                "a.four",
                Outcome::Fail {
                    found: "x".to_owned(),
                    expected: "y".to_owned(),
                },
            ),
            result(
                "a.five",
                Outcome::NotApplicable {
                    reason: "no fact".to_owned(),
                },
            ),
        ]);
        assert_eq!(report.score.passed, 3);
        assert_eq!(report.score.failed, 1);
        assert_eq!(report.score.not_applicable, 1);
        assert_eq!(report.score.errored, 0);
        assert_eq!(report.score.applicable, 4);
        assert_eq!(report.score.basis_points, 7_500);
    }

    #[test]
    fn an_empty_report_scores_full_marks() {
        let score = Score::of(&[]);
        assert_eq!(score.applicable, 0);
        assert_eq!(score.basis_points, 10_000);
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let report = report(vec![
            result("ssh.root_login", Outcome::Pass),
            result(
                "mac.enforcing",
                Outcome::Fail {
                    found: "permissive".to_owned(),
                    expected: "enforcing".to_owned(),
                },
            ),
        ]);
        let json = report.to_json().unwrap();
        assert_eq!(Report::from_json(&json).unwrap(), report);
        assert!(Report::from_json("not json").is_err());
    }

    #[test]
    fn text_summary_names_every_control() {
        let report = report(vec![
            result("ssh.root_login", Outcome::Pass),
            result(
                "mac.enforcing",
                Outcome::Fail {
                    found: "permissive".to_owned(),
                    expected: "enforcing".to_owned(),
                },
            ),
        ]);
        let text = report.render_text();
        assert!(text.contains("ssh.root_login"));
        assert!(text.contains("mac.enforcing"));
        assert!(text.contains("expected enforcing, found permissive"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn diff_reports_improved_and_regressed_in_id_order() {
        let before = report(vec![
            result("ssh.root_login", Outcome::Pass),
            result(
                "mac.enforcing",
                Outcome::Fail {
                    found: "permissive".to_owned(),
                    expected: "enforcing".to_owned(),
                },
            ),
        ]);
        let after = report(vec![
            result(
                "ssh.root_login",
                Outcome::Fail {
                    found: "yes".to_owned(),
                    expected: "no".to_owned(),
                },
            ),
            result("mac.enforcing", Outcome::Pass),
        ]);
        let delta = diff(&before, &after);
        assert_eq!(delta.changes.len(), 2);
        assert_eq!(delta.changes[0].id.as_str(), "mac.enforcing");
        assert_eq!(delta.changes[0].direction, Direction::Improved);
        assert_eq!(delta.changes[1].id.as_str(), "ssh.root_login");
        assert_eq!(delta.changes[1].direction, Direction::Regressed);
    }

    #[test]
    fn diff_of_identical_reports_is_empty() {
        let report = report(vec![result("ssh.root_login", Outcome::Pass)]);
        assert!(diff(&report, &report).is_empty());
    }

    #[test]
    fn diff_reports_added_and_removed_controls() {
        let before = report(vec![result("ssh.root_login", Outcome::Pass)]);
        let after = report(vec![result("mac.enforcing", Outcome::Pass)]);
        let delta = diff(&before, &after);
        assert_eq!(delta.changes.len(), 2);
        assert_eq!(delta.changes[0].id.as_str(), "mac.enforcing");
        assert_eq!(delta.changes[0].direction, Direction::Added);
        assert_eq!(delta.changes[1].id.as_str(), "ssh.root_login");
        assert_eq!(delta.changes[1].direction, Direction::Removed);
    }

    #[test]
    fn a_fact_key_is_not_needed_to_build_a_report() {
        // Compile-time check that `FactKey` is public and usable by callers.
        assert!(FactKey::try_new("ssh.enabled").is_ok());
    }
}
