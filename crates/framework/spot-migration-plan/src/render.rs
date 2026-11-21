//! Rendering a plan for a human: plain text for a terminal and Markdown for the review screen
//! and the audit record. Both are deterministic and group the changes by risk class.

use std::fmt::Write as _;

use crate::change::{BlockingClass, Change};
use crate::plan::ReleasePlan;
use crate::risk::Risk;

impl ReleasePlan {
    /// The plan as plain text: a summary line, the risk, the estimate, then the changes grouped
    /// by class.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Release plan: metadata {} -> {}",
            self.from_version, self.to_version
        );
        let _ = writeln!(out, "Risk: {}", risk_text(self.risk));
        let _ = writeln!(out, "Estimate: {}", estimate_text(self));
        let _ = writeln!(out, "{}", summary_text(self));
        for risk in [Risk::Destructive, Risk::NeedsReview, Risk::Additive] {
            let changes: Vec<&Change> = self.changes.iter().filter(|c| c.risk == risk).collect();
            if changes.is_empty() {
                continue;
            }
            let _ = writeln!(out);
            let _ = writeln!(out, "{} ({})", risk_title(risk), changes.len());
            for change in changes {
                let _ = writeln!(out, "  - {}", change_line(change));
                if let Some(reason) = &change.reason {
                    let _ = writeln!(out, "      why: {reason}");
                }
                if let Some(alternative) = &change.alternative {
                    let _ = writeln!(out, "      instead: {alternative}");
                }
            }
        }
        out
    }

    /// The plan as Markdown: a summary, the risk, the estimate, then the changes grouped by
    /// class.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# Release plan: metadata {} -> {}",
            self.from_version, self.to_version
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "- **Risk:** {}", risk_text(self.risk));
        let _ = writeln!(out, "- **Estimate:** {}", estimate_text(self));
        let _ = writeln!(out, "- **Changes:** {}", summary_text(self));
        for risk in [Risk::Destructive, Risk::NeedsReview, Risk::Additive] {
            let changes: Vec<&Change> = self.changes.iter().filter(|c| c.risk == risk).collect();
            if changes.is_empty() {
                continue;
            }
            let _ = writeln!(out);
            let _ = writeln!(out, "## {} ({})", risk_title(risk), changes.len());
            for change in changes {
                let _ = writeln!(out, "- {}", change_line(change));
                if let Some(reason) = &change.reason {
                    let _ = writeln!(out, "  - why: {reason}");
                }
                if let Some(alternative) = &change.alternative {
                    let _ = writeln!(out, "  - instead: {alternative}");
                }
            }
        }
        out
    }
}

/// The summary line's tail: the change count by area.
fn summary_text(plan: &ReleasePlan) -> String {
    let count = |area| plan.changes_in(area).len();
    format!(
        "{} {} (schema {}, metadata {}, patches {}, behaviour {})",
        plan.changes.len(),
        if plan.changes.len() == 1 {
            "change"
        } else {
            "changes"
        },
        count(crate::change::Area::Schema),
        count(crate::change::Area::Metadata),
        count(crate::change::Area::Patch),
        count(crate::change::Area::Behaviour),
    )
}

/// The estimate as text.
fn estimate_text(plan: &ReleasePlan) -> String {
    format!(
        "{}s expected ({}s low, {}s high), blocking: {}{}",
        plan.estimate.expected,
        plan.estimate.low,
        plan.estimate.high,
        blocking_text(plan.estimate.blocking),
        if plan.estimate.unknown {
            ", some statistics unknown"
        } else {
            ""
        }
    )
}

/// One change as a line.
fn change_line(change: &Change) -> String {
    let mut line = format!(
        "[{}] {} — {}",
        change.area.as_str(),
        change.target,
        change.description
    );
    if let (Some(before), Some(after)) = (&change.before, &change.after) {
        let _ = write!(line, " ({before} -> {after})");
    }
    line
}

/// A risk's human text.
pub(crate) fn risk_text(risk: Risk) -> &'static str {
    match risk {
        Risk::Additive => "additive",
        Risk::NeedsReview => "needs review",
        Risk::Destructive => "destructive",
    }
}

/// A risk's title for a group.
fn risk_title(risk: Risk) -> &'static str {
    match risk {
        Risk::Additive => "Additive",
        Risk::NeedsReview => "Needs review",
        Risk::Destructive => "Destructive",
    }
}

/// A blocking class's human text.
fn blocking_text(blocking: BlockingClass) -> &'static str {
    match blocking {
        BlockingClass::Online => "online",
        BlockingClass::BriefLock => "brief lock",
        BlockingClass::MaintenanceWindow => "maintenance window",
    }
}
