//! The catalogue as a Markdown table, for the lead to place under `docs/`.

use std::fmt::Write as _;

use spot_conditions::Millis;

use crate::entries;

fn duration(after: Millis) -> String {
    let minutes = after.as_u64() / 60_000;
    if minutes >= 60 && minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

fn list(items: Vec<String>) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

/// The catalogue as a Markdown table, one row per condition in the ADR's order.
///
/// Generated from the same data the registry is built from, so the document cannot drift from
/// the code; the crate does not write the file.
#[must_use]
pub fn conditions_markdown() -> String {
    let mut out = String::from(
        "| Condition | Severity | Subject | Escalation | Automatic response | Operator commands | Debounce |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    for entry in entries() {
        let def = &entry.def;
        let escalation = def
            .escalation
            .iter()
            .map(|step| format!("{} after {}", step.to.as_str(), duration(step.after_millis)))
            .collect();
        let response = def
            .response
            .iter()
            .map(|step| format!("{} x{}", step.action, step.limit))
            .collect();
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} | {} | {} |",
            def.id,
            def.severity.as_str(),
            def.subject_kind,
            list(escalation),
            list(response),
            list(def.actions.clone()),
            duration(entry.debounce),
        );
    }
    out
}
