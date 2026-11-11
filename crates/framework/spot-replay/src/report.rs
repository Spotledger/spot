//! The replay report: what the sample covered and every outcome that differed, structured (JSON)
//! and rendered (text).

use serde::{Deserialize, Serialize};

use crate::budget::BudgetStop;
use crate::divergence::{Divergence, DivergenceClass, DivergenceKind};
use crate::journal::Cursor;
use crate::policy::{ReplayPolicy, Stratum};

/// What one replay covered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// How many candidates the journal held for the window.
    pub considered: usize,
    /// How many the policy selected.
    pub selected: usize,
    /// How many were actually re-executed.
    pub replayed: u64,
    /// What each `(DocType, sensitivity)` stratum contributed.
    pub strata: Vec<Stratum>,
    /// Whether the whole sample was replayed. `false` when the budget stopped the run.
    pub complete: bool,
    /// Which budget bound stopped the run, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<BudgetStop>,
    /// How long the run took, in microseconds, on the monotonic clock.
    pub elapsed_micros: u64,
}

impl Coverage {
    /// Whether every selected entry was replayed.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// The share of the sample that was replayed, in basis points (0–10_000).
    #[must_use]
    pub fn share_basis_points(&self) -> u32 {
        if self.selected == 0 {
            return 10_000;
        }
        let replayed = self.replayed.min(self.selected as u64);
        u32::try_from(replayed * 10_000 / self.selected as u64).unwrap_or(10_000)
    }
}

/// The verdict of a replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayOutcome {
    /// Every replayed operation produced exactly the recorded outcome.
    Clean,
    /// Differences, all of them claimed by a declared behaviour entry.
    ExpectedChanges,
    /// At least one difference nothing declared.
    Unexplained,
    /// The budget stopped the run before the sample was replayed through.
    Partial,
}

/// Everything one replay produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayReport {
    /// The site.
    pub site: String,
    /// The clone's label.
    pub clone: String,
    /// The sample policy that was applied.
    pub policy: ReplayPolicy,
    /// What the run covered.
    pub coverage: Coverage,
    /// Every outcome that differed.
    pub divergences: Vec<Divergence>,
    /// Where to resume, when the budget stopped the run short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// The verdict.
    pub outcome: ReplayOutcome,
}

impl ReplayReport {
    /// The unexplained divergences.
    pub fn unexplained(&self) -> impl Iterator<Item = &Divergence> {
        self.divergences
            .iter()
            .filter(|d| d.class == DivergenceClass::Unexplained)
    }

    /// The divergences a declared behaviour entry claimed.
    pub fn expected(&self) -> impl Iterator<Item = &Divergence> {
        self.divergences
            .iter()
            .filter(|d| d.class == DivergenceClass::Expected)
    }

    /// The one-minor-unit rounding divergences.
    pub fn rounding(&self) -> impl Iterator<Item = &Divergence> {
        self.divergences.iter().filter(|d| d.rounding)
    }

    /// The divergences of one kind.
    pub fn of_kind(&self, kind: DivergenceKind) -> impl Iterator<Item = &Divergence> {
        self.divergences.iter().filter(move |d| d.kind == kind)
    }

    /// The verdict for the given coverage and divergences.
    #[must_use]
    pub fn outcome_of(coverage: &Coverage, divergences: &[Divergence]) -> ReplayOutcome {
        if divergences
            .iter()
            .any(|d| d.class == DivergenceClass::Unexplained)
        {
            return ReplayOutcome::Unexplained;
        }
        if !coverage.complete {
            return ReplayOutcome::Partial;
        }
        if divergences.is_empty() {
            ReplayOutcome::Clean
        } else {
            ReplayOutcome::ExpectedChanges
        }
    }

    /// The report as pretty JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// The report as text for a terminal or a review comment.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Behavioural replay on site {} (clone {})\n",
            self.site, self.clone
        ));
        out.push_str(&format!(
            "  policy      : {} seed={} per_stratum={}\n",
            window_name(&self.policy),
            self.policy.seed,
            self.policy.per_stratum
        ));
        out.push_str(&format!(
            "  coverage    : {}/{} selected, {} replayed ({}%){}\n",
            self.coverage.selected,
            self.coverage.considered,
            self.coverage.replayed,
            self.coverage.share_basis_points() / 100,
            match self.coverage.stop {
                Some(stop) => format!(", stopped by {}", stop.as_str()),
                None => String::new(),
            }
        ));
        for stratum in &self.coverage.strata {
            out.push_str(&format!(
                "    {:?} {}: {}/{} selected\n",
                stratum.sensitivity, stratum.doctype, stratum.selected, stratum.available
            ));
        }
        if self.divergences.is_empty() {
            out.push_str("  divergences : none\n");
        } else {
            out.push_str(&format!("  divergences : {}\n", self.divergences.len()));
            for divergence in &self.divergences {
                let class = match divergence.class {
                    DivergenceClass::Expected => {
                        format!(
                            "expected ({})",
                            divergence.declared_by.as_deref().unwrap_or("?")
                        )
                    }
                    DivergenceClass::Unexplained => "UNEXPLAINED".to_owned(),
                };
                let rounding = if divergence.rounding {
                    " [rounding]"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "    {} {} {} {}: {} -> {} ({}){}{}\n",
                    divergence.source,
                    divergence.sequence,
                    divergence.doctype,
                    divergence.field,
                    divergence.before,
                    divergence.after,
                    match divergence.delta {
                        Some(delta) => delta.to_string(),
                        None => "-".to_owned(),
                    },
                    class,
                    rounding
                ));
            }
        }
        if let Some(cursor) = &self.cursor {
            out.push_str(&format!(
                "  resume from : {} #{}\n",
                cursor.source, cursor.sequence
            ));
        }
        out.push_str(&format!("  outcome     : {}\n", outcome_name(self.outcome)));
        out
    }
}

/// The window's name for the report.
fn window_name(policy: &ReplayPolicy) -> String {
    match policy.window {
        crate::policy::Window::LastDays { days } => format!("last {days} days"),
        crate::policy::Window::LastDocuments { documents } => {
            format!("last {documents} documents")
        }
    }
}

/// The verdict's name.
fn outcome_name(outcome: ReplayOutcome) -> &'static str {
    match outcome {
        ReplayOutcome::Clean => "clean",
        ReplayOutcome::ExpectedChanges => "expected changes only",
        ReplayOutcome::Unexplained => "UNEXPLAINED divergences",
        ReplayOutcome::Partial => "partial (budget reached)",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::divergence::{Divergence, DivergenceClass};

    fn divergence(class: DivergenceClass, rounding: bool) -> Divergence {
        Divergence {
            doctype: "invoice".to_owned(),
            record: "r1".to_owned(),
            source: "s".to_owned(),
            sequence: 1,
            kind: DivergenceKind::Amount,
            field: "totals.tax".to_owned(),
            before: "100".to_owned(),
            after: "101".to_owned(),
            delta: Some(1),
            rounding,
            class,
            declared_by: match class {
                DivergenceClass::Expected => Some("behaviour/rounding".to_owned()),
                DivergenceClass::Unexplained => None,
            },
        }
    }

    fn coverage(complete: bool) -> Coverage {
        Coverage {
            considered: 10,
            selected: 10,
            replayed: if complete { 10 } else { 4 },
            strata: Vec::new(),
            complete,
            stop: if complete {
                None
            } else {
                Some(BudgetStop::Documents)
            },
            elapsed_micros: 1_000,
        }
    }

    #[test]
    fn no_divergences_is_clean() {
        assert_eq!(
            ReplayReport::outcome_of(&coverage(true), &[]),
            ReplayOutcome::Clean
        );
    }

    #[test]
    fn an_unexplained_divergence_wins_over_partial() {
        let divs = vec![divergence(DivergenceClass::Unexplained, true)];
        assert_eq!(
            ReplayReport::outcome_of(&coverage(false), &divs),
            ReplayOutcome::Unexplained
        );
    }

    #[test]
    fn an_expected_only_run_is_expected_changes() {
        let divs = vec![divergence(DivergenceClass::Expected, true)];
        assert_eq!(
            ReplayReport::outcome_of(&coverage(true), &divs),
            ReplayOutcome::ExpectedChanges
        );
    }

    #[test]
    fn a_partial_run_without_divergences_is_partial() {
        assert_eq!(
            ReplayReport::outcome_of(&coverage(false), &[]),
            ReplayOutcome::Partial
        );
    }

    #[test]
    fn the_share_is_full_when_nothing_was_selected() {
        let mut c = coverage(true);
        c.selected = 0;
        c.replayed = 0;
        assert_eq!(c.share_basis_points(), 10_000);
    }
}
