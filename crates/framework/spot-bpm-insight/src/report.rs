//! The report definitions the report engine serves, so insight shows up as ordinary reports.
//!
//! The definitions are [`ReportDef`] values in `spot-reports`' format (WP-PLT-14): TOML over a
//! base DocType, with typed parameters, grouping and aggregates. They run as ordinary reports —
//! permission-filtered and field-checked by the report engine — so no bespoke endpoint is
//! needed for cycle time by step, the stuck queue or throughput by week.
//!
//! # The projected DocTypes (an integration point)
//!
//! A report resolves against the site's metadata, so the BPM layer projects the engine's durable
//! records as DocTypes for the report engine to query. These definitions read two of them:
//!
//! - [`NODE_RUN_DOCTYPE`] (`bpm_node_run`): one row per node attempt, with `process`,
//!   `process_version`, `node`, `node_kind`, `run`, `state`, `owner`, `started_at`, `ended_at`,
//!   `cycle_ms`, `work_ms`, `wait_ms` and `failure_code`.
//! - [`RUN_DOCTYPE`] (`bpm_case`): one row per run, with `process`, `process_version`, `status`,
//!   `started_at`, `ended_at` and `week` (the ISO week of `ended_at`).
//!
//! The DocType and field names are the BPM projection's to own; this crate fixes the shape the
//! reports need and the host supplies the metadata (see the report's `## Open questions`).
//!
//! Titles and column headings come from the [`RunLabels`] pair (ADR-080): the stuck-queue report
//! is titled "Stuck {plural}" and its run column is headed with the singular label.

use spot_reports::ReportDef;

use crate::error::{InsightError, InsightResult};
use crate::labels::RunLabels;

/// The report name for cycle time by step.
pub const CYCLE_TIME_BY_STEP: &str = "bpm_cycle_time_by_step";
/// The report name for the stuck queue.
pub const STUCK_QUEUE: &str = "bpm_stuck_queue";
/// The report name for throughput by week.
pub const THROUGHPUT_BY_WEEK: &str = "bpm_throughput_by_week";

/// The projected DocType holding one row per node attempt.
pub const NODE_RUN_DOCTYPE: &str = "bpm_node_run";
/// The projected DocType holding one row per run.
pub const RUN_DOCTYPE: &str = "bpm_case";

/// The cycle-time-by-step definition, as shipped TOML.
const CYCLE_TIME_BY_STEP_TOML: &str = r#"
name = "bpm_cycle_time_by_step"
label = "Cycle time by step"
base = "bpm_node_run"
limit = 500
max_rows = 5000
group_by = ["process", "process_version", "node", "node_kind"]

[[columns]]
field = "process"

[[columns]]
field = "process_version"

[[columns]]
field = "node"

[[columns]]
field = "node_kind"

[[columns]]
field = "attempt"
label = "Completed Requests"
aggregate = "count"

[[columns]]
field = "cycle_ms"
label = "Average cycle (ms)"
aggregate = "avg"

[[columns]]
field = "wait_ms"
label = "Longest wait (ms)"
aggregate = "max"

[[columns]]
field = "work_ms"
label = "Total work (ms)"
aggregate = "sum"

[[order_by]]
field = "cycle_ms"
dir = "desc"
"#;

/// The stuck-queue definition, as shipped TOML. Its title and run column heading are overwritten
/// from the [`RunLabels`] pair.
const STUCK_QUEUE_TOML: &str = r#"
name = "bpm_stuck_queue"
label = "Stuck Requests"
base = "bpm_node_run"
limit = 1000
max_rows = 10000

[[columns]]
field = "run"
label = "Request"

[[columns]]
field = "process"

[[columns]]
field = "process_version"

[[columns]]
field = "node"

[[columns]]
field = "node_kind"

[[columns]]
field = "owner"

[[columns]]
field = "state"

[[columns]]
field = "started_at"

[filters]
all = [
  { field = "state", op = "ne", value = "done" },
  { field = "state", op = "ne", value = "failed" },
]

[[order_by]]
field = "started_at"
dir = "asc"
"#;

/// The throughput-by-week definition, as shipped TOML.
const THROUGHPUT_BY_WEEK_TOML: &str = r#"
name = "bpm_throughput_by_week"
label = "Throughput by week"
base = "bpm_case"
limit = 500
max_rows = 5000
group_by = ["process", "process_version", "week"]

[[columns]]
field = "process"

[[columns]]
field = "process_version"

[[columns]]
field = "week"

[[columns]]
field = "ended_at"
label = "Completed Requests"
aggregate = "count"

[[order_by]]
field = "week"
dir = "desc"
"#;

/// The three report definitions insight ships, with their titles and run column headings taken
/// from `labels`.
///
/// # Errors
/// [`InsightError::ReportDefinition`] when a shipped definition fails to parse or to validate;
/// the message names the report and the offending key.
pub fn report_definitions(labels: &RunLabels) -> InsightResult<Vec<ReportDef>> {
    let mut defs = vec![
        parse(CYCLE_TIME_BY_STEP, CYCLE_TIME_BY_STEP_TOML)?,
        parse(STUCK_QUEUE, STUCK_QUEUE_TOML)?,
        parse(THROUGHPUT_BY_WEEK, THROUGHPUT_BY_WEEK_TOML)?,
    ];
    for def in &mut defs {
        match def.name.as_str() {
            STUCK_QUEUE => {
                def.label = format!("Stuck {}", labels.plural());
                set_column_label(def, "run", labels.singular());
            }
            CYCLE_TIME_BY_STEP => {
                set_column_label(def, "attempt", &format!("Completed {}", labels.plural()));
            }
            THROUGHPUT_BY_WEEK => {
                set_column_label(def, "ended_at", &format!("Completed {}", labels.plural()));
            }
            _ => {}
        }
    }
    for def in &defs {
        let problems = def.validate();
        if !problems.is_empty() {
            let detail = problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(InsightError::ReportDefinition {
                name: def.name.clone(),
                detail,
            });
        }
    }
    Ok(defs)
}

/// Sets the label of the column with `field`, when the definition has one.
fn set_column_label(def: &mut ReportDef, field: &str, label: &str) {
    for column in &mut def.columns {
        if column.field == field {
            column.label = Some(label.to_owned());
        }
    }
}

fn parse(name: &str, toml: &str) -> InsightResult<ReportDef> {
    ReportDef::from_toml(toml).map_err(|error| InsightError::ReportDefinition {
        name: name.to_owned(),
        detail: error.to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_definition_parses_and_validates() {
        let defs = report_definitions(&RunLabels::defaults()).unwrap();
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, CYCLE_TIME_BY_STEP);
        assert_eq!(defs[1].name, STUCK_QUEUE);
        assert_eq!(defs[2].name, THROUGHPUT_BY_WEEK);
        for def in &defs {
            assert!(def.validate().is_empty(), "{} validates", def.name);
        }
    }

    #[test]
    fn titles_and_headings_come_from_the_label_pair() {
        let labels = RunLabels::new(Some("Ticket"), Some("Tickets")).unwrap();
        let defs = report_definitions(&labels).unwrap();
        let stuck = defs.iter().find(|def| def.name == STUCK_QUEUE).unwrap();
        assert_eq!(stuck.label, "Stuck Tickets");
        let run_column = stuck
            .columns
            .iter()
            .find(|column| column.field == "run")
            .unwrap();
        assert_eq!(run_column.label.as_deref(), Some("Ticket"));
        let cycle = defs
            .iter()
            .find(|def| def.name == CYCLE_TIME_BY_STEP)
            .unwrap();
        let completed = cycle
            .columns
            .iter()
            .find(|column| column.field == "attempt")
            .unwrap();
        assert_eq!(completed.label.as_deref(), Some("Completed Tickets"));
    }

    #[test]
    fn no_shipped_title_or_heading_says_case() {
        for def in report_definitions(&RunLabels::defaults()).unwrap() {
            assert!(!def.label.contains("Case"), "{} label", def.name);
            for column in &def.columns {
                if let Some(label) = &column.label {
                    assert!(!label.contains("Case"), "{} column label", def.name);
                }
            }
        }
    }
}
