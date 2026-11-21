//! The report draft's edits: the [`ReportOp`] command log, the pure [`apply_op`] and the
//! [`ReportEditor`] that makes undo and redo exact.
//!
//! This module is the edit half of [`crate::report`]; the draft types and their own methods
//! live there. Every op is one logical step a designer takes, and an op that fails changes
//! nothing.

use serde::{Deserialize, Serialize};

use crate::chart::ChartSpec;
use crate::error::OpError;
use crate::report::{
    ABSOLUTE_MAX_ROWS, Aggregate, DEFAULT_LIMIT, DEFAULT_MAX_ROWS, Direction, DraftColumn,
    DraftFilter, DraftJoin, DraftParamType, DraftParameter, DraftSort, DraftStatus, FilterTarget,
    FilterValue, REPORT_MAX_COLUMNS, REPORT_MAX_JOINS, ReportDraft, is_name,
};

/// One edit of a report draft, applied by [`apply_op`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReportOp {
    /// Adds a column.
    AddColumn {
        /// The field path.
        field: String,
        /// Its heading.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Its aggregate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregate: Option<Aggregate>,
        /// A presentation hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// Removes a column.
    RemoveColumn {
        /// The field path.
        field: String,
    },
    /// Sets a column's heading.
    SetColumnLabel {
        /// The field path.
        field: String,
        /// The heading; `None` returns to the field's name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Sets or clears a column's aggregate.
    SetColumnAggregate {
        /// The field path.
        field: String,
        /// The aggregate; `None` shows the plain value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregate: Option<Aggregate>,
    },
    /// Declares a link.
    AddJoin {
        /// The alias.
        alias: String,
        /// The record type it points at.
        doctype: String,
        /// The link field it follows.
        on: String,
    },
    /// Removes a link.
    RemoveJoin {
        /// The alias.
        alias: String,
    },
    /// Adds a value the viewer may supply.
    AddParameter {
        /// The parameter.
        parameter: DraftParameter,
    },
    /// Removes a value the viewer may supply.
    RemoveParameter {
        /// The parameter's name.
        name: String,
    },
    /// Adds a condition every row must meet.
    AddFilter {
        /// The condition.
        filter: DraftFilter,
    },
    /// Removes the condition at `index`.
    RemoveFilter {
        /// The condition's position.
        index: usize,
    },
    /// Groups the report by a field.
    GroupBy {
        /// The field path.
        field: String,
    },
    /// Stops grouping by a field.
    UngroupBy {
        /// The field path.
        field: String,
    },
    /// Adds a sort key, replacing any sort on the same field.
    SortBy {
        /// The column name or field path.
        field: String,
        /// The direction.
        direction: Direction,
    },
    /// Removes every sort key.
    ClearSort,
    /// Sets the footer totals, by column name.
    SetTotals {
        /// The columns totalled.
        columns: Vec<String>,
    },
    /// Sets or clears the chart.
    SetChart {
        /// The chart; `None` removes it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chart: Option<ChartSpec>,
    },
    /// Sets the row limits.
    SetLimit {
        /// The default limit; `None` uses the platform default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        /// The largest limit a run honours.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_rows: Option<u32>,
    },
    /// Sets the roles that may open the report.
    SetRoles {
        /// The roles.
        roles: Vec<String>,
    },
    /// Replaces the base record type, clearing everything that named the old one.
    SetBase {
        /// The new base record type.
        doctype: String,
    },
    /// Sets the report's heading.
    SetLabel {
        /// The heading.
        label: String,
    },
    /// Sets where the draft is in its life.
    SetStatus {
        /// The status.
        status: DraftStatus,
    },
}

impl ReportOp {
    /// The op's stable name, for logs and audit.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AddColumn { .. } => "add_column",
            Self::RemoveColumn { .. } => "remove_column",
            Self::SetColumnLabel { .. } => "set_column_label",
            Self::SetColumnAggregate { .. } => "set_column_aggregate",
            Self::AddJoin { .. } => "add_join",
            Self::RemoveJoin { .. } => "remove_join",
            Self::AddParameter { .. } => "add_parameter",
            Self::RemoveParameter { .. } => "remove_parameter",
            Self::AddFilter { .. } => "add_filter",
            Self::RemoveFilter { .. } => "remove_filter",
            Self::GroupBy { .. } => "group_by",
            Self::UngroupBy { .. } => "ungroup_by",
            Self::SortBy { .. } => "sort_by",
            Self::ClearSort => "clear_sort",
            Self::SetTotals { .. } => "set_totals",
            Self::SetChart { .. } => "set_chart",
            Self::SetLimit { .. } => "set_limit",
            Self::SetRoles { .. } => "set_roles",
            Self::SetBase { .. } => "set_base",
            Self::SetLabel { .. } => "set_label",
            Self::SetStatus { .. } => "set_status",
        }
    }
}

/// Applies one op to `draft`, returning the changed draft. Pure: `draft` is untouched, and an
/// error changes nothing.
///
/// # Errors
///
/// An [`OpError`] naming the precondition that failed.
pub fn apply_op(draft: &ReportDraft, op: &ReportOp) -> Result<ReportDraft, OpError> {
    let mut next = draft.clone();
    match op {
        ReportOp::AddColumn {
            field,
            label,
            aggregate,
            format,
        } => {
            check_field(field)?;
            if next.columns.len() >= REPORT_MAX_COLUMNS {
                return Err(OpError::BadValue {
                    message: format!(
                        "A report may show at most {REPORT_MAX_COLUMNS} columns. Remove one first."
                    ),
                });
            }
            let column = DraftColumn {
                field: field.clone(),
                label: label.clone(),
                aggregate: *aggregate,
                format: format.clone(),
            };
            if next.columns.iter().any(|c| c.name() == column.name()) {
                return Err(OpError::Duplicate {
                    name: column.name(),
                });
            }
            next.columns.push(column);
        }
        ReportOp::RemoveColumn { field } => {
            let before = next.columns.len();
            next.columns.retain(|c| &c.field != field);
            if next.columns.len() == before {
                return Err(OpError::UnknownColumn {
                    field: field.clone(),
                });
            }
            let name = field.replace('.', "_");
            next.totals.retain(|t| t != &name);
        }
        ReportOp::SetColumnLabel { field, label } => {
            let Some(column) = next.columns.iter_mut().find(|c| &c.field == field) else {
                return Err(OpError::UnknownColumn {
                    field: field.clone(),
                });
            };
            column.label = label.clone();
        }
        ReportOp::SetColumnAggregate { field, aggregate } => {
            let Some(column) = next.columns.iter_mut().find(|c| &c.field == field) else {
                return Err(OpError::UnknownColumn {
                    field: field.clone(),
                });
            };
            column.aggregate = *aggregate;
        }
        ReportOp::AddJoin { alias, doctype, on } => {
            check_name(alias)?;
            check_name(doctype)?;
            check_field(on)?;
            if alias == spot_reports::definition::BASE_ALIAS {
                return Err(OpError::InvalidName {
                    name: alias.clone(),
                });
            }
            if next.join(alias).is_some() {
                return Err(OpError::Duplicate {
                    name: alias.clone(),
                });
            }
            if next.joins.len() >= REPORT_MAX_JOINS {
                return Err(OpError::BadValue {
                    message: format!(
                        "A report may follow at most {REPORT_MAX_JOINS} links. Remove one first."
                    ),
                });
            }
            next.joins.push(DraftJoin {
                alias: alias.clone(),
                doctype: doctype.clone(),
                on: on.clone(),
            });
        }
        ReportOp::RemoveJoin { alias } => {
            let before = next.joins.len();
            next.joins.retain(|j| &j.alias != alias);
            if next.joins.len() == before {
                return Err(OpError::UnknownJoin {
                    alias: alias.clone(),
                });
            }
            let prefix = format!("{alias}.");
            next.columns.retain(|c| !c.field.starts_with(&prefix));
            next.filters.retain(|f| !f.field.starts_with(&prefix));
            next.group_by.retain(|g| !g.starts_with(&prefix));
            next.sort.retain(|s| !s.field.starts_with(&prefix));
        }
        ReportOp::AddParameter { parameter } => {
            check_parameter(parameter)?;
            if next.parameter(&parameter.name).is_some() {
                return Err(OpError::Duplicate {
                    name: parameter.name.clone(),
                });
            }
            next.parameters.push(parameter.clone());
        }
        ReportOp::RemoveParameter { name } => {
            let before = next.parameters.len();
            next.parameters.retain(|p| &p.name != name);
            if next.parameters.len() == before {
                return Err(OpError::UnknownParameter { name: name.clone() });
            }
            next.filters.retain(
                |f| !matches!(&f.target, FilterTarget::Parameter { name: used } if used == name),
            );
        }
        ReportOp::AddFilter { filter } => {
            check_field(&filter.field)?;
            if let FilterTarget::Parameter { name } = &filter.target
                && next.parameter(name).is_none()
            {
                return Err(OpError::UnknownParameter { name: name.clone() });
            }
            if let FilterTarget::Value { value } = &filter.target {
                value
                    .to_toml()
                    .map_err(|message| OpError::BadValue { message })?;
            }
            next.filters.push(filter.clone());
        }
        ReportOp::RemoveFilter { index } => {
            if *index >= next.filters.len() {
                return Err(OpError::BadValue {
                    message: "That condition is not in the report any more.".to_owned(),
                });
            }
            next.filters.remove(*index);
        }
        ReportOp::GroupBy { field } => {
            check_field(field)?;
            if next
                .columns
                .iter()
                .any(|c| &c.field == field && c.is_aggregate())
            {
                return Err(OpError::BadType {
                    message: "A summary column cannot also be what the report groups by."
                        .to_owned(),
                });
            }
            if !next.group_by.iter().any(|g| g == field) {
                next.group_by.push(field.clone());
            }
        }
        ReportOp::UngroupBy { field } => {
            next.group_by.retain(|g| g != field);
        }
        ReportOp::SortBy { field, direction } => {
            check_field(field)?;
            next.sort.retain(|s| &s.field != field);
            next.sort.push(DraftSort {
                field: field.clone(),
                direction: *direction,
                nulls: None,
            });
        }
        ReportOp::ClearSort => next.sort.clear(),
        ReportOp::SetTotals { columns } => {
            let names = next.column_names();
            for name in columns {
                if !names.iter().any(|n| n == name) {
                    return Err(OpError::UnknownColumn {
                        field: name.clone(),
                    });
                }
            }
            next.totals = columns.clone();
        }
        ReportOp::SetChart { chart } => {
            if let Some(chart) = chart {
                let issue = chart.validate(&next.column_names(), "chart");
                if let Some(first) = issue.into_iter().next() {
                    return Err(OpError::BadType {
                        message: first.message,
                    });
                }
            }
            next.chart = chart.clone();
        }
        ReportOp::SetLimit { limit, max_rows } => {
            let default = limit.unwrap_or(DEFAULT_LIMIT);
            let max = max_rows.unwrap_or(DEFAULT_MAX_ROWS);
            if !(1..=ABSOLUTE_MAX_ROWS).contains(&default) {
                return Err(OpError::BadValue {
                    message: format!(
                        "A report may show between 1 and {ABSOLUTE_MAX_ROWS} rows at a time."
                    ),
                });
            }
            if !(1..=ABSOLUTE_MAX_ROWS).contains(&max) {
                return Err(OpError::BadValue {
                    message: format!("The row limit must be between 1 and {ABSOLUTE_MAX_ROWS}."),
                });
            }
            if max < default {
                return Err(OpError::BadValue {
                    message: "The largest row limit cannot be below the usual one.".to_owned(),
                });
            }
            next.limit = *limit;
            next.max_rows = *max_rows;
        }
        ReportOp::SetRoles { roles } => {
            next.roles = roles.clone();
        }
        ReportOp::SetBase { doctype } => {
            check_name(doctype)?;
            next.base = doctype.clone();
            next.columns.clear();
            next.joins.clear();
            next.filters.clear();
            next.group_by.clear();
            next.sort.clear();
            next.totals.clear();
            next.chart = None;
        }
        ReportOp::SetLabel { label } => {
            if label.trim().is_empty() {
                return Err(OpError::BadValue {
                    message: "A report needs a title.".to_owned(),
                });
            }
            next.label = label.clone();
        }
        ReportOp::SetStatus { status } => {
            next.status = *status;
        }
    }
    Ok(next)
}

/// A draft and its op log: undo and redo move a cursor over the states the log produced.
///
/// The editor keeps every state, so undo is exact and redo replays the same op;
/// [`ReportEditor::replayed`] recomputes the current state from the initial draft and the log,
/// which is what proves [`apply_op`] pure.
#[derive(Clone, Debug)]
pub struct ReportEditor {
    initial: ReportDraft,
    states: Vec<ReportDraft>,
    ops: Vec<ReportOp>,
    cursor: usize,
}

impl ReportEditor {
    /// An editor over `draft`, with no edits.
    #[must_use]
    pub fn new(draft: ReportDraft) -> Self {
        Self {
            states: vec![draft.clone()],
            initial: draft,
            ops: Vec::new(),
            cursor: 0,
        }
    }

    /// The current draft.
    #[must_use]
    pub fn current(&self) -> &ReportDraft {
        self.states.get(self.cursor).unwrap_or(&self.initial)
    }

    /// Applies an op, dropping any redoable future.
    ///
    /// # Errors
    ///
    /// The [`OpError`] the op raised; nothing changes.
    pub fn apply(&mut self, op: ReportOp) -> Result<&ReportDraft, OpError> {
        let next = apply_op(self.current(), &op)?;
        self.states.truncate(self.cursor + 1);
        self.ops.truncate(self.cursor);
        self.states.push(next);
        self.ops.push(op);
        self.cursor += 1;
        Ok(self.current())
    }

    /// Whether undo would move back.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    /// Whether redo would move forward.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.cursor < self.ops.len()
    }

    /// Moves back one op. Returns whether it moved.
    pub fn undo(&mut self) -> bool {
        if !self.can_undo() {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Moves forward one op. Returns whether it moved.
    pub fn redo(&mut self) -> bool {
        if !self.can_redo() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// The ops applied so far, in order (the command log).
    #[must_use]
    pub fn log(&self) -> &[ReportOp] {
        &self.ops[..self.cursor]
    }

    /// Every op the editor ever applied, including redoable ones.
    #[must_use]
    pub fn ops(&self) -> &[ReportOp] {
        &self.ops
    }

    /// How many ops the log holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cursor
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cursor == 0
    }

    /// The current draft recomputed from the initial draft and the log. Equal to
    /// [`current`](ReportEditor::current) whenever [`apply_op`] is pure.
    ///
    /// # Errors
    ///
    /// The [`OpError`] a logged op raises on replay (which would mean the log was tampered
    /// with).
    pub fn replayed(&self) -> Result<ReportDraft, OpError> {
        let mut draft = self.initial.clone();
        for op in self.log() {
            draft = apply_op(&draft, op)?;
        }
        Ok(draft)
    }
}

fn check_name(text: &str) -> Result<(), OpError> {
    if is_name(text) {
        Ok(())
    } else {
        Err(OpError::InvalidName {
            name: text.to_owned(),
        })
    }
}

fn check_field(field: &str) -> Result<(), OpError> {
    if field.trim().is_empty() {
        return Err(OpError::BadValue {
            message: "Choose a field first.".to_owned(),
        });
    }
    let (alias, name) = match field.split_once('.') {
        Some((alias, name)) => (Some(alias), name),
        None => (None, field),
    };
    if let Some(alias) = alias {
        check_name(alias)?;
    }
    check_name(name)
}

fn check_parameter(parameter: &DraftParameter) -> Result<(), OpError> {
    check_name(&parameter.name)?;
    if parameter.label.trim().is_empty() {
        return Err(OpError::BadValue {
            message: "Give the value a label the person filling it in can read.".to_owned(),
        });
    }
    if parameter.required && parameter.default.is_some() {
        return Err(OpError::BadValue {
            message: "A required value cannot also have a default.".to_owned(),
        });
    }
    match parameter.ty {
        DraftParamType::Select => {
            if parameter.options.is_empty() {
                return Err(OpError::BadValue {
                    message: "A choice needs at least one option.".to_owned(),
                });
            }
            if let Some(default) = &parameter.default {
                let FilterValue::Text { text } = default else {
                    return Err(OpError::BadType {
                        message: "A choice's default must be one of its options.".to_owned(),
                    });
                };
                if !parameter.options.iter().any(|o| o == text) {
                    return Err(OpError::BadValue {
                        message: "A choice's default must be one of its options.".to_owned(),
                    });
                }
            }
        }
        DraftParamType::Link => {
            let Some(doctype) = &parameter.doctype else {
                return Err(OpError::BadValue {
                    message: "A record value needs the kind of record it points at.".to_owned(),
                });
            };
            check_name(doctype)?;
        }
        DraftParamType::Text
        | DraftParamType::Int
        | DraftParamType::Decimal
        | DraftParamType::Date => {}
    }
    if let Some(default) = &parameter.default {
        default
            .to_toml()
            .map_err(|message| OpError::BadValue { message })?;
    }
    Ok(())
}
