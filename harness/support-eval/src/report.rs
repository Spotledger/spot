//! The Markdown report: a readable summary of one run with a per-incident trace.
//!
//! The report is deterministic for a result (no clock, no random ids) and contains no payloads:
//! trace entries carry the action and its typed detail, not the untrusted text an incident
//! carried. The Python renderer in `tools/lints/support_gate/report.py` produces the same shape
//! from the JSON, for CI.

use std::fmt::Write as _;

use crate::metrics::{EvalResult, IncidentOutcome, IncidentResult};

/// Renders the result as Markdown.
#[must_use]
pub fn render(result: &EvalResult) -> String {
    let mut out = String::new();
    let summary = &result.summary;
    let _ = writeln!(out, "# Support evaluation: `{}`", result.suite);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Generated at: `{}` (unix seconds, supplied by the caller)",
        result.generated_at
    );
    let _ = writeln!(out);
    if summary.gate_passed {
        let _ = writeln!(
            out,
            "**Gate: PASS** — harm {}, unreversed {}, fix rate {}.{:02}% (floor {}.{:02}%).",
            summary.harm,
            summary.unreversed,
            summary.fix_rate_bp / 100,
            summary.fix_rate_bp % 100,
            summary.min_fix_rate_bp / 100,
            summary.min_fix_rate_bp % 100,
        );
    } else {
        let _ = writeln!(
            out,
            "**Gate: BLOCKED** — harm {}, unreversed {}, fix rate {}.{:02}% (floor {}.{:02}%).",
            summary.harm,
            summary.unreversed,
            summary.fix_rate_bp / 100,
            summary.fix_rate_bp % 100,
            summary.min_fix_rate_bp / 100,
            summary.min_fix_rate_bp % 100,
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "| --- | --- |");
    let _ = writeln!(out, "| Incidents | {} |", summary.total);
    let _ = writeln!(
        out,
        "| Expected to be fixed | {} |",
        summary.expected_resolved
    );
    let _ = writeln!(
        out,
        "| Expected to be escalated | {} |",
        summary.expected_escalated
    );
    let _ = writeln!(
        out,
        "| Resolved / escalated / failed | {} / {} / {} |",
        summary.resolved, summary.escalated, summary.failed
    );
    let _ = writeln!(
        out,
        "| Escalations correct | {} of {} |",
        summary.escalation_correct, summary.expected_escalated
    );
    let _ = writeln!(
        out,
        "| Mean actions per incident | {}.{:03} |",
        summary.mean_actions_milli / 1_000,
        summary.mean_actions_milli % 1_000
    );
    let _ = writeln!(out, "| Harm | {} |", summary.harm);
    let _ = writeln!(out, "| Unreversed changes | {} |", summary.unreversed);
    let _ = writeln!(out);
    for incident in &result.incidents {
        render_incident(&mut out, incident);
    }
    out
}

/// Renders one incident section.
fn render_incident(out: &mut String, incident: &IncidentResult) {
    let _ = writeln!(out, "## `{}`: {}", incident.id, incident.title);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- Category: `{}` · tier `{}` · expected `{}` · outcome **{}**",
        incident.category,
        incident.tier,
        incident.expected,
        incident.outcome.as_str()
    );
    let _ = writeln!(
        out,
        "- Fix state: {} · resolution matched: {} · actions: {}",
        incident.fix_state.as_deref().unwrap_or("none"),
        incident.resolution_matched,
        incident.actions
    );
    if incident.harm.is_empty() {
        let _ = writeln!(out, "- Harm: none");
    } else {
        let _ = writeln!(out, "- Harm: {}", incident.harm.join("; "));
    }
    let _ = writeln!(out, "- Unreversed changes: {}", incident.unreversed);
    if !incident.injection_surfaces.is_empty() {
        let _ = writeln!(
            out,
            "- Untrusted surfaces: {}",
            incident.injection_surfaces.join(", ")
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "| # | Tool | Tenant | Status | Detail |");
    let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
    for entry in &incident.trace {
        let _ = writeln!(
            out,
            "| {} | `{}` | `{}` | `{}` | {} |",
            entry.seq,
            entry.tool,
            entry.tenant,
            entry.status.as_str(),
            entry.detail.replace('|', "\\|")
        );
    }
    if incident.audit.is_empty() {
        return;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Audit:");
    for entry in &incident.audit {
        let _ = writeln!(
            out,
            "- `{}` -> `{}`: {}",
            entry.action, entry.state, entry.detail
        );
    }
}

/// The one-line verdict the command line prints.
#[must_use]
pub fn verdict(result: &EvalResult) -> String {
    let summary = &result.summary;
    let blocked = result
        .incidents
        .iter()
        .filter(|incident| incident.outcome == IncidentOutcome::Failed)
        .count();
    format!(
        "{}: {} incident(s), {} resolved, {} escalated, {} failed, harm {}, unreversed {}, fix rate {}.{:02}%",
        if summary.gate_passed {
            "PASS"
        } else {
            "BLOCKED"
        },
        summary.total,
        summary.resolved,
        summary.escalated,
        blocked,
        summary.harm,
        summary.unreversed,
        summary.fix_rate_bp / 100,
        summary.fix_rate_bp % 100
    )
}
