//! A plain-language summary of a run, for the console and the support view.
//!
//! [`explain`] turns a [`MigrationRun`] into an [`Explanation`]: what changed, who approved,
//! what the rehearsal found and what happened, in sentences an operator can read. It is pure —
//! it reads no clock, no store and no environment — and it never names a secret.

use crate::run::{ApprovalDecision, MigrationRun, Outcome, RehearsalVerdict};

/// A run explained in plain language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    /// One line naming the run, its actor and its outcome.
    pub headline: String,
    /// What the plan changes.
    pub what_changed: Vec<String>,
    /// Who approved or rejected, in order.
    pub who_approved: Vec<String>,
    /// What the rehearsal found, when one ran.
    pub rehearsal: Option<String>,
    /// What happened, in one sentence.
    pub what_happened: String,
    /// How long the run took.
    pub timing: String,
}

impl Explanation {
    /// The explanation as plain text, one line per section.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines = vec![self.headline.clone()];
        if !self.what_changed.is_empty() {
            lines.push(format!("Changes: {}", self.what_changed.join(" ")));
        }
        if !self.who_approved.is_empty() {
            lines.push(format!("Approvals: {}", self.who_approved.join("; ")));
        }
        if let Some(rehearsal) = &self.rehearsal {
            lines.push(format!("Rehearsal: {rehearsal}"));
        }
        lines.push(format!("Result: {}", self.what_happened));
        lines.push(self.timing.clone());
        lines.join("\n")
    }
}

/// Explains one run in plain language.
#[must_use]
pub fn explain(run: &MigrationRun) -> Explanation {
    Explanation {
        headline: format!(
            "Migration run {} by {} — {}.",
            run.id,
            run.actor,
            outcome_sentence(run.outcome)
        ),
        what_changed: changes(run),
        who_approved: approvals(run),
        rehearsal: run.rehearsal.as_ref().map(|report| {
            format!(
                "the rehearsal on the clone found {}; {} finding(s); report {}.",
                verdict_sentence(report.verdict),
                report.findings,
                short_digest(&report.artifact.digest)
            )
        }),
        what_happened: happened(run),
        timing: timing(run),
    }
}

fn changes(run: &MigrationRun) -> Vec<String> {
    let summary = &run.plan.summary;
    if summary.changes == 0 {
        return vec!["the plan changes nothing.".to_owned()];
    }
    let mut parts = Vec::new();
    for (label, count) in [
        ("schema", summary.schema),
        ("metadata", summary.metadata),
        ("patches", summary.patches),
        ("behaviour", summary.behaviour),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    vec![format!(
        "the plan changes {} thing(s) ({}); risk {}.",
        summary.changes,
        parts.join(", "),
        if summary.risk.is_empty() {
            "unknown"
        } else {
            &summary.risk
        }
    )]
}

fn approvals(run: &MigrationRun) -> Vec<String> {
    run.approvals
        .iter()
        .map(|approval| {
            let verb = match approval.decision {
                ApprovalDecision::Approved => "approved",
                ApprovalDecision::Rejected => "rejected",
            };
            format!(
                "{} ({}) {verb} at {}",
                approval.approver, approval.role, approval.at
            )
        })
        .collect()
}

fn happened(run: &MigrationRun) -> String {
    let journal = match &run.apply_journal_ref {
        Some(journal) => format!(" Apply journal {}.", short_digest(&journal.digest)),
        None => String::new(),
    };
    match run.outcome {
        Outcome::Planned => format!("The plan is recorded; apply has not started.{journal}"),
        Outcome::Rehearsed => format!("The rehearsal is recorded; apply has not started.{journal}"),
        Outcome::Applied => format!("Apply ran; verify has not finished.{journal}"),
        Outcome::Verified => format!("Verify passed and users were admitted.{journal}"),
        Outcome::Failed => format!("The run failed and the site was restored.{journal}"),
        Outcome::RolledBack => format!("The run was rolled back to the old version.{journal}"),
        Outcome::Refused => format!("The run was refused before apply.{journal}"),
    }
}

fn timing(run: &MigrationRun) -> String {
    match (run.ended, run.duration_micros()) {
        (Some(ended), Some(duration)) => {
            format!(
                "Started at {} µs, ended at {ended} µs ({duration} µs).",
                run.started
            )
        }
        _ => format!("Started at {} µs; still running.", run.started),
    }
}

fn verdict_sentence(verdict: RehearsalVerdict) -> &'static str {
    match verdict {
        RehearsalVerdict::Clean => "no difference and every invariant held",
        RehearsalVerdict::ExpectedChanges => {
            "only differences the plan declares, and every invariant held"
        }
        RehearsalVerdict::UnexplainedDifferences => "a difference the plan does not explain",
        RehearsalVerdict::InvariantsFailed => "an invariant set that failed",
    }
}

fn outcome_sentence(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Planned => "planned",
        Outcome::Rehearsed => "rehearsed",
        Outcome::Applied => "applied, not yet verified",
        Outcome::Verified => "verified",
        Outcome::Failed => "failed",
        Outcome::RolledBack => "rolled back",
        Outcome::Refused => "refused",
    }
}

fn short_digest(digest: &str) -> String {
    digest.chars().take(12).collect()
}
