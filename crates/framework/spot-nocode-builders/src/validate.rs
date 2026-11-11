//! Draft-level validation, before any metadata is resolved: names, duplicates, references
//! between the draft's own parts, limits, charts and totals. Every problem is reported at
//! once, in plain language, with the fix a designer should make.
//!
//! Field existence and type fit are the report engine's business (it resolves the effective
//! metadata); [`crate::compile::plan`] turns its refusals into the same [`Issue`] shape.

use crate::codes;
use crate::dashboard::{DashboardDraft, RefreshPolicy, TileKind, TileSource};
use crate::error::Issue;
use crate::report::{
    ABSOLUTE_MAX_ROWS, DEFAULT_LIMIT, DEFAULT_MAX_ROWS, DraftParamType, FilterTarget,
    REPORT_MAX_COLUMNS, REPORT_MAX_JOINS, ReportDraft, is_name,
};

/// Every structural problem with a report draft.
#[must_use]
pub fn report_issues(draft: &ReportDraft) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !is_name(&draft.name) {
        issues.push(Issue::at(
            codes::BAD_NAME,
            "name",
            "This report's name is not a valid name.",
            "Use lower-case letters, digits and underscores, starting with a letter.",
        ));
    }
    if draft.label.trim().is_empty() {
        issues.push(Issue::at(
            codes::BAD_VALUE,
            "label",
            "This report has no title.",
            "Give the report a title a person can read.",
        ));
    }
    if !is_name(&draft.base) {
        issues.push(Issue::at(
            codes::BAD_NAME,
            "base",
            "This report is not built on a valid kind of record.",
            "Choose the kind of record the report lists.",
        ));
    }
    if draft.columns.is_empty() {
        issues.push(Issue::error(
            codes::EMPTY,
            "This report has nothing to show yet.",
            "Add at least one column.",
        ));
    }
    if draft.columns.len() > REPORT_MAX_COLUMNS {
        issues.push(Issue::error(
            codes::TOO_LARGE,
            "This report shows more columns than a report may hold.",
            "Remove columns until the report is within the limit.",
        ));
    }

    let mut names: Vec<String> = Vec::new();
    for (index, column) in draft.columns.iter().enumerate() {
        let at = format!("columns[{index}]");
        if column.field.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A column has no field.",
                "Choose the field the column shows.",
            ));
        }
        let name = column.name();
        if names.contains(&name) {
            issues.push(Issue::at(
                codes::DUPLICATE,
                &at,
                "Two columns show the same field.",
                "Remove one of them, or summarise it differently.",
            ));
        }
        names.push(name);
    }

    let mut aliases: Vec<String> = vec!["base".to_owned()];
    for (index, join) in draft.joins.iter().enumerate() {
        let at = format!("joins[{index}]");
        if !is_name(&join.alias) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                &at,
                "A link's name is not a valid name.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            ));
        }
        if !is_name(&join.doctype) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                &at,
                "A link does not point at a valid kind of record.",
                "Choose the kind of record the link points at.",
            ));
        }
        if join.on.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A link does not say which field connects the records.",
                "Choose the field that links the two kinds of record.",
            ));
        }
        if aliases.contains(&join.alias) {
            issues.push(Issue::at(
                codes::DUPLICATE,
                &at,
                "Two links share a name.",
                "Give each link its own name.",
            ));
        }
        aliases.push(join.alias.clone());
    }
    if draft.joins.len() > REPORT_MAX_JOINS {
        issues.push(Issue::error(
            codes::TOO_LARGE,
            "This report follows more links than a report may follow.",
            "Remove links until the report is within the limit.",
        ));
    }

    let mut parameters: Vec<String> = Vec::new();
    for (index, parameter) in draft.parameters.iter().enumerate() {
        let at = format!("parameters[{index}]");
        if !is_name(&parameter.name) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                &at,
                "A value's name is not a valid name.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            ));
        }
        if parameter.label.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A value has no label.",
                "Give it a label the person filling it in can read.",
            ));
        }
        if parameters.contains(&parameter.name) {
            issues.push(Issue::at(
                codes::DUPLICATE,
                &at,
                "Two values share a name.",
                "Give each value its own name.",
            ));
        }
        parameters.push(parameter.name.clone());
        if parameter.required && parameter.default.is_some() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A required value also has a default, so it is never really required.",
                "Remove the default, or stop requiring the value.",
            ));
        }
        match parameter.ty {
            DraftParamType::Select => {
                if parameter.options.is_empty() {
                    issues.push(Issue::at(
                        codes::BAD_VALUE,
                        &at,
                        "A choice has no options to choose from.",
                        "Add at least one option.",
                    ));
                }
                if let Some(default) = &parameter.default {
                    let ok = matches!(default, crate::report::FilterValue::Text { text }
                        if parameter.options.iter().any(|o| o == text));
                    if !ok {
                        issues.push(Issue::at(
                            codes::BAD_TYPE,
                            &at,
                            "A choice's default is not one of its options.",
                            "Choose one of the options as the default.",
                        ));
                    }
                }
            }
            DraftParamType::Link => match &parameter.doctype {
                Some(doctype) if is_name(doctype) => {}
                _ => issues.push(Issue::at(
                    codes::BAD_VALUE,
                    &at,
                    "A record value does not say which kind of record it points at.",
                    "Choose the kind of record the value points at.",
                )),
            },
            DraftParamType::Text
            | DraftParamType::Int
            | DraftParamType::Decimal
            | DraftParamType::Date => {}
        }
        if let Some(default) = &parameter.default
            && let Err(message) = default.to_toml()
        {
            issues.push(Issue::at(
                codes::BAD_TYPE,
                &at,
                message,
                "Enter the value again.",
            ));
        }
    }

    for (index, filter) in draft.filters.iter().enumerate() {
        let at = format!("filters[{index}]");
        if filter.field.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A condition does not say which field it checks.",
                "Choose the field the condition checks.",
            ));
        }
        match &filter.target {
            FilterTarget::Parameter { name } => {
                if !parameters.contains(name) {
                    issues.push(Issue::at(
                        codes::UNKNOWN_PARAMETER,
                        &at,
                        "A condition uses a value the report does not offer.",
                        "Add the value, or choose one the report already offers.",
                    ));
                }
            }
            FilterTarget::Value { value } => {
                if let Err(message) = value.to_toml() {
                    issues.push(Issue::at(
                        codes::BAD_TYPE,
                        &at,
                        message,
                        "Enter the value again.",
                    ));
                }
            }
        }
    }

    let mut seen_groups: Vec<String> = Vec::new();
    for (index, field) in draft.group_by.iter().enumerate() {
        let at = format!("group_by[{index}]");
        if field.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A grouping has no field.",
                "Choose the field the report groups by.",
            ));
        }
        if seen_groups.contains(field) {
            issues.push(Issue::at(
                codes::DUPLICATE,
                &at,
                "Two groupings use the same field.",
                "Remove the repeated grouping.",
            ));
        }
        seen_groups.push(field.clone());
        if draft
            .columns
            .iter()
            .any(|c| &c.field == field && c.is_aggregate())
        {
            issues.push(Issue::at(
                codes::BAD_TYPE,
                &at,
                "A summary column cannot also be what the report groups by.",
                "Group by the field itself, not by its summary.",
            ));
        }
    }

    let aggregates = draft.columns.iter().filter(|c| c.is_aggregate()).count();
    if aggregates > 0 {
        if draft.group_by.is_empty() && aggregates < draft.columns.len() {
            issues.push(Issue::error(
                codes::BAD_TYPE,
                "This report summarises some columns but does not say what to group by.",
                "Choose the fields the summary is grouped by, or summarise every column.",
            ));
        }
        for (index, column) in draft.columns.iter().enumerate() {
            if !column.is_aggregate() && !draft.group_by.contains(&column.field) {
                issues.push(Issue::at(
                    codes::BAD_TYPE,
                    format!("columns[{index}]"),
                    "This column is shown row by row, but the report is a summary.",
                    "Group by this field, or summarise the column.",
                ));
            }
        }
    }

    for (index, sort) in draft.sort.iter().enumerate() {
        if sort.field.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                format!("sort[{index}]"),
                "A sort key has no field.",
                "Choose the column the report sorts by.",
            ));
        }
    }

    for total in &draft.totals {
        if !names.contains(total) {
            issues.push(Issue::at(
                codes::UNKNOWN_COLUMN,
                "totals",
                "A total names a column the report does not have.",
                "Choose one of the report's columns for the total.",
            ));
        }
    }

    let default = draft.limit.unwrap_or(DEFAULT_LIMIT);
    let maximum = draft.max_rows.unwrap_or(DEFAULT_MAX_ROWS);
    if !(1..=ABSOLUTE_MAX_ROWS).contains(&default) {
        issues.push(Issue::at(
            codes::BAD_VALUE,
            "limit",
            "The usual row limit is outside the range a report allows.",
            "Choose a limit between 1 and the largest a report allows.",
        ));
    }
    if !(1..=ABSOLUTE_MAX_ROWS).contains(&maximum) {
        issues.push(Issue::at(
            codes::BAD_VALUE,
            "max_rows",
            "The largest row limit is outside the range a report allows.",
            "Choose a limit between 1 and the largest a report allows.",
        ));
    }
    if maximum < default {
        issues.push(Issue::at(
            codes::BAD_VALUE,
            "max_rows",
            "The largest row limit is below the usual one, so the usual one can never be used.",
            "Raise the largest limit, or lower the usual one.",
        ));
    }

    if let Some(chart) = &draft.chart {
        issues.extend(chart.validate(&names, "chart"));
    }

    if draft.roles.is_empty() {
        issues.push(Issue::warning(
            codes::NOT_READY,
            "roles",
            "Only the platform can open this report until you choose who may see it.",
            "Choose the roles that may open the report.",
        ));
    }
    issues
}

/// Every structural problem with a dashboard draft (the reports themselves are checked when
/// the dashboard compiles).
#[must_use]
pub fn dashboard_issues(draft: &DashboardDraft) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !is_name(&draft.name) {
        issues.push(Issue::at(
            codes::BAD_NAME,
            "name",
            "This dashboard's name is not a valid name.",
            "Use lower-case letters, digits and underscores, starting with a letter.",
        ));
    }
    if draft.label.trim().is_empty() {
        issues.push(Issue::at(
            codes::BAD_VALUE,
            "label",
            "This dashboard has no title.",
            "Give the dashboard a title a person can read.",
        ));
    }
    if !(1..=24).contains(&draft.grid.columns) {
        issues.push(Issue::at(
            codes::BAD_LAYOUT,
            "grid",
            "The dashboard's grid has a width the builder does not accept.",
            "Choose between 1 and 24 columns.",
        ));
    }
    if draft.grid.rows == 0 {
        issues.push(Issue::at(
            codes::BAD_LAYOUT,
            "grid",
            "The dashboard's grid has no rows.",
            "Give the grid at least one row.",
        ));
    }
    if draft.tiles.is_empty() {
        issues.push(Issue::error(
            codes::EMPTY,
            "This dashboard has no tiles yet.",
            "Add at least one tile.",
        ));
    }
    match draft.refresh {
        RefreshPolicy::Manual | RefreshPolicy::OnOpen => {}
        RefreshPolicy::Every { seconds: 0 } => issues.push(Issue::at(
            codes::BAD_REFRESH,
            "refresh",
            "The dashboard refreshes every zero seconds.",
            "Choose a refresh interval, or refresh only when the viewer asks.",
        )),
        RefreshPolicy::Every { .. } => {}
    }

    let mut ids: Vec<String> = Vec::new();
    for (index, tile) in draft.tiles.iter().enumerate() {
        let at = format!("tiles[{index}]");
        if !is_name(&tile.id) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                &at,
                "A tile's name is not a valid name.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            ));
        }
        if ids.contains(&tile.id) {
            issues.push(Issue::at(
                codes::DUPLICATE,
                &at,
                "Two tiles share a name.",
                "Give each tile its own name.",
            ));
        }
        ids.push(tile.id.clone());
        if tile.title.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A tile has no title.",
                "Give the tile a title a person can read.",
            ));
        }
        if !is_name(tile.source.report()) {
            issues.push(Issue::at(
                codes::UNKNOWN_REPORT,
                &at,
                "A tile does not name a report.",
                "Choose the report the tile shows.",
            ));
        }
        if tile.place.width == 0 || tile.place.height == 0 || tile.place.row == 0 {
            issues.push(Issue::at(
                codes::BAD_LAYOUT,
                &at,
                "A tile has no place on the grid.",
                "Give the tile a row, a column and at least one cell.",
            ));
        }
        if tile.place.column == 0
            || tile.place.column > draft.grid.columns
            || tile.place.last_column() > draft.grid.columns
        {
            issues.push(Issue::at(
                codes::BAD_LAYOUT,
                &at,
                "A tile is outside the dashboard's grid.",
                "Move the tile inside the grid, or make the grid wider.",
            ));
        }
        if tile.place.last_row() > draft.grid.rows {
            issues.push(Issue::at(
                codes::BAD_LAYOUT,
                &at,
                "A tile is below the dashboard's grid.",
                "Move the tile up, or make the grid taller.",
            ));
        }
        for other in draft.tiles.iter().skip(index + 1) {
            if tile.place.overlaps(&other.place) {
                issues.push(Issue::at(
                    codes::BAD_LAYOUT,
                    &at,
                    "Two tiles sit on top of each other.",
                    "Move one of them to a free place.",
                ));
            }
        }
        if let TileSource::List { columns, .. } = &tile.source
            && columns.is_empty()
        {
            issues.push(Issue::at(
                codes::EMPTY,
                &at,
                "A list tile has no columns.",
                "Choose the columns the list shows.",
            ));
        }
        if tile.source.kind() == TileKind::Chart
            && let TileSource::Chart { chart, .. } = &tile.source
        {
            issues.extend(chart.validate(&[], &at).into_iter().filter(|issue| {
                // The report's own columns are checked at compile time; only the structural
                // chart rules can be checked here, so an unknown column is left out.
                issue.code != codes::UNKNOWN_COLUMN
            }));
        }
    }

    let mut filters: Vec<String> = Vec::new();
    for (index, filter) in draft.filters.iter().enumerate() {
        let at = format!("filters[{index}]");
        if !is_name(&filter.name) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                &at,
                "A dashboard filter's name is not a valid name.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            ));
        }
        if filter.label.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                &at,
                "A dashboard filter has no label.",
                "Give it a label the viewer can read.",
            ));
        }
        if filters.contains(&filter.name) {
            issues.push(Issue::at(
                codes::DUPLICATE,
                &at,
                "Two dashboard filters share a name.",
                "Give each filter its own name.",
            ));
        }
        filters.push(filter.name.clone());
        if let Some(default) = &filter.default
            && let Err(message) = default.to_toml()
        {
            issues.push(Issue::at(
                codes::BAD_TYPE,
                &at,
                message,
                "Enter the value again.",
            ));
        }
    }

    if draft.roles.is_empty() {
        issues.push(Issue::warning(
            codes::NOT_READY,
            "roles",
            "Only the platform can open this dashboard until you choose who may see it.",
            "Choose the roles that may open the dashboard.",
        ));
    }
    issues
}
