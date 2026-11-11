//! A preview a builder may run: bounded by a row cap and a time cap, never unrestricted.
//!
//! The preview compiles exactly like the published report and runs through the same
//! [`ReportRunner`], so it obeys the viewer's rights; the only difference is that it always
//! carries a small row cap and the host checks the time it took. The sample rows a builder
//! canvas shows while nothing has been run yet are generated here too — deterministic, by
//! column, with no data read at all.

use serde_json::{Value as Json, json};
use spot_permissions::Principal;
use spot_reports::{CompiledReport, ReportArgs, ReportRunner, RunOutput};

use crate::codes;
use crate::error::Issue;
use crate::report::{Aggregate, ReportDraft};

/// What a preview may cost: at most this many rows and, as the host measures it, this many
/// milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewBudget {
    /// The most rows the preview returns; zero is refused.
    pub max_rows: u32,
    /// The most milliseconds the run may take; zero is refused.
    pub max_millis: u64,
}

impl PreviewBudget {
    /// A budget of `max_rows` rows and `max_millis` milliseconds.
    #[must_use]
    pub fn new(max_rows: u32, max_millis: u64) -> Self {
        Self {
            max_rows,
            max_millis,
        }
    }

    /// The budget a builder's canvas uses by default: twenty rows, five seconds.
    #[must_use]
    pub fn canvas() -> Self {
        Self::new(20, 5_000)
    }

    /// Checks the budget itself can bound a run.
    ///
    /// # Errors
    ///
    /// [`codes::PREVIEW_BUDGET`] when either cap is zero.
    pub fn check(&self) -> Result<(), Issue> {
        if self.max_rows == 0 {
            return Err(Issue::error(
                codes::PREVIEW_BUDGET,
                "A preview needs a row limit, or it would show everything.",
                "Start the preview with a small number of rows.",
            ));
        }
        if self.max_millis == 0 {
            return Err(Issue::error(
                codes::PREVIEW_BUDGET,
                "A preview needs a time limit, or it could run for ever.",
                "Give the preview a few seconds.",
            ));
        }
        Ok(())
    }
}

/// A preview run: what it returned and the cap it was held to.
#[derive(Clone, Debug)]
pub struct PreviewRun {
    /// The report's result and messages, exactly as a published run returns them.
    pub output: RunOutput,
    /// The row cap the run was held to.
    pub cap: u32,
}

/// Runs `report` for `principal` under `budget`. The run is always capped; the time is checked
/// afterwards with [`check_time`], because the clock is the host's, never this crate's.
///
/// # Errors
///
/// [`codes::PREVIEW_BUDGET`] for an unusable budget, or the report engine's refusal.
pub async fn preview(
    runner: &ReportRunner,
    report: &CompiledReport,
    principal: &Principal,
    args: &ReportArgs,
    budget: &PreviewBudget,
) -> Result<PreviewRun, Vec<Issue>> {
    budget.check().map_err(|issue| vec![issue])?;
    let cap = budget.max_rows.min(report.max_rows).max(1);
    let output = runner
        .run(report, principal, args, Some(u64::from(cap)))
        .await
        .map_err(|error| {
            vec![
                Issue::error(
                    codes::BAD_VALUE,
                    "The preview could not run this report.",
                    "Check the report's conditions and values, then try again.",
                )
                .from_source(error.code()),
            ]
        })?;
    Ok(PreviewRun { output, cap })
}

/// Whether `elapsed_millis`, as the host measured it, fits the budget.
///
/// # Errors
///
/// [`codes::PREVIEW_TOO_SLOW`] when the run took longer than the budget allows.
pub fn check_time(budget: &PreviewBudget, elapsed_millis: u64) -> Result<(), Issue> {
    budget.check()?;
    if elapsed_millis > budget.max_millis {
        return Err(Issue::error(
            codes::PREVIEW_TOO_SLOW,
            "The preview took longer than it is allowed to, so it was stopped.",
            "Narrow the report with a condition, or ask for a larger preview.",
        ));
    }
    Ok(())
}

/// `count` sample rows for a builder's canvas, generated from the draft's columns. The same
/// draft always gives the same rows, and nothing is read.
#[must_use]
pub fn sample_rows(draft: &ReportDraft, count: usize) -> Vec<Vec<Json>> {
    (0..count)
        .map(|row| {
            draft
                .columns
                .iter()
                .map(|column| sample_cell(column, row))
                .collect()
        })
        .collect()
}

fn sample_cell(column: &crate::report::DraftColumn, row: usize) -> Json {
    match column.aggregate {
        Some(Aggregate::Count) => json!(row + 1),
        Some(Aggregate::Sum | Aggregate::Avg) => json!("100.00"),
        Some(Aggregate::Min | Aggregate::Max) => json!("sample"),
        None => match column.format.as_deref() {
            Some("date") => json!("2026-01-01"),
            Some("money") => json!("10.00"),
            Some("percent") => json!("50"),
            _ => json!(format!("{}-{}", column.name(), row + 1)),
        },
    }
}
