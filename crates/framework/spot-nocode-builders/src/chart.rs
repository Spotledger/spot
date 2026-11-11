//! The chart a report or a dashboard tile draws: a single number, a bar, a line, a pie or a
//! plain table.
//!
//! A chart is a **description over the report's own columns**, never a query of its own: it
//! names the column on the horizontal axis and the columns whose values are drawn. The client
//! draws it; this crate only checks that the shape has the columns it needs, in plain
//! language.

use serde::{Deserialize, Serialize};

use crate::codes;
use crate::error::Issue;

/// The shape a chart draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    /// One large number: exactly one value column and no horizontal axis.
    Number,
    /// Bars per horizontal value.
    Bar,
    /// A line across the horizontal values.
    Line,
    /// A pie with one slice per horizontal value: exactly one value column.
    Pie,
    /// A plain table of the selected columns.
    Table,
}

impl ChartKind {
    /// The name as written in a draft.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Bar => "bar",
            Self::Line => "line",
            Self::Pie => "pie",
            Self::Table => "table",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "number" => Some(Self::Number),
            "bar" => Some(Self::Bar),
            "line" => Some(Self::Line),
            "pie" => Some(Self::Pie),
            "table" => Some(Self::Table),
            _ => None,
        }
    }
}

/// One column the chart draws.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartSeries {
    /// The report column whose values are drawn.
    pub column: String,
    /// The legend label; the column's own label when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ChartSeries {
    /// A series over one column.
    #[must_use]
    pub fn new(column: &str) -> Self {
        Self {
            column: column.to_owned(),
            label: None,
        }
    }

    /// The same series with a legend label.
    #[must_use]
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }
}

/// The chart a report carries into a dashboard, and the chart a chart tile draws.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartSpec {
    /// The shape.
    pub kind: ChartKind,
    /// The column on the horizontal axis; absent for a number and a table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// The value columns drawn.
    #[serde(default)]
    pub series: Vec<ChartSeries>,
    /// A heading; the tile's or report's title when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl ChartSpec {
    /// One big number from `column`.
    #[must_use]
    pub fn number(column: &str) -> Self {
        Self {
            kind: ChartKind::Number,
            x: None,
            series: vec![ChartSeries::new(column)],
            title: None,
        }
    }

    /// A bar chart: one bar per `x` value, one bar group per series column.
    #[must_use]
    pub fn bar(x: &str, series: Vec<ChartSeries>) -> Self {
        Self {
            kind: ChartKind::Bar,
            x: Some(x.to_owned()),
            series,
            title: None,
        }
    }

    /// A line chart.
    #[must_use]
    pub fn line(x: &str, series: Vec<ChartSeries>) -> Self {
        Self {
            kind: ChartKind::Line,
            x: Some(x.to_owned()),
            series,
            title: None,
        }
    }

    /// A pie chart from `x` and one value column.
    #[must_use]
    pub fn pie(x: &str, series: ChartSeries) -> Self {
        Self {
            kind: ChartKind::Pie,
            x: Some(x.to_owned()),
            series: vec![series],
            title: None,
        }
    }

    /// A plain table.
    #[must_use]
    pub fn table() -> Self {
        Self {
            kind: ChartKind::Table,
            x: None,
            series: Vec::new(),
            title: None,
        }
    }

    /// The same chart with a heading.
    #[must_use]
    pub fn with_title(mut self, title: &str) -> Self {
        self.title = Some(title.to_owned());
        self
    }

    /// Every column the chart reads, the axis column first.
    #[must_use]
    pub fn columns(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(x) = &self.x {
            out.push(x.clone());
        }
        for series in &self.series {
            if !out.contains(&series.column) {
                out.push(series.column.clone());
            }
        }
        out
    }

    /// Checks the chart against the report's column names. Every problem is reported at once.
    #[must_use]
    pub fn validate(&self, columns: &[String], at: &str) -> Vec<Issue> {
        let mut issues = Vec::new();
        let known = |name: &str| columns.iter().any(|c| c == name);
        let unknown = || {
            Issue::at(
                codes::UNKNOWN_COLUMN,
                at,
                "This chart uses a column the report no longer has. Pick the column again.",
                "Choose one of the report's own columns for the chart.",
            )
        };

        match self.kind {
            ChartKind::Number => {
                if self.x.is_some() {
                    issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "A single-number chart has no horizontal axis.",
                        "Remove the axis column, or choose a bar, line or pie chart.",
                    ));
                }
                if self.series.len() != 1 {
                    issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "A single-number chart shows exactly one value.",
                        "Choose one column to show as the number.",
                    ));
                }
            }
            ChartKind::Bar | ChartKind::Line | ChartKind::Pie => {
                match &self.x {
                    None => issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "This chart needs a column for its horizontal axis.",
                        "Choose the column whose values label the bars, points or slices.",
                    )),
                    Some(x) if !known(x) => issues.push(unknown()),
                    Some(_) => {}
                }
                if self.series.is_empty() {
                    issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "This chart needs at least one value to draw.",
                        "Choose one or more columns to draw.",
                    ));
                }
                if self.kind == ChartKind::Pie && self.series.len() > 1 {
                    issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "A pie chart shows one value per slice.",
                        "Choose a single value column for the pie, or use a bar chart.",
                    ));
                }
                for series in &self.series {
                    if !known(&series.column) {
                        issues.push(unknown());
                    } else if self.x.as_deref() == Some(series.column.as_str()) {
                        issues.push(Issue::at(
                            codes::BAD_CHART,
                            at,
                            "The axis column cannot also be a value column.",
                            "Pick a different value column.",
                        ));
                    }
                }
            }
            ChartKind::Table => {
                if self.x.is_some() {
                    issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "A table shows every column; it has no separate axis.",
                        "Remove the axis column.",
                    ));
                }
                if !self.series.is_empty() {
                    issues.push(Issue::at(
                        codes::BAD_CHART,
                        at,
                        "A table shows every column; it does not draw selected values.",
                        "Remove the selected values, or choose a bar, line or pie chart.",
                    ));
                }
            }
        }
        for series in &self.series {
            if !known(&series.column) {
                // One report per unknown series is enough; the loop above already added one
                // for the shapes that draw values, so this keeps the check complete for a
                // number chart with a stale column.
                if self.kind == ChartKind::Number {
                    issues.push(unknown());
                }
            }
        }
        issues
    }
}
