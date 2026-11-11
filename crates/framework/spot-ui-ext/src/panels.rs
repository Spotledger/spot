//! The panel half of validation: blocks, their data queries and the restricted markdown subset.

use crate::code::{UiExtError, codes};
use crate::expr::Scope;
use crate::facts::QueryKind;
use crate::markdown;
use crate::model::{Block, Column, Panel};
use crate::validate::{Cx, check_destination, check_expression, fields_scope, report};

pub(crate) fn check_panel(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    panel: &Panel,
) {
    let (fields, what) = fields_scope(cx.facts, panel.doctype.as_deref());
    let scope = Scope::Fields(fields.as_deref());
    for (position, block) in panel.blocks.iter().enumerate() {
        let at = format!("{ptr}/blocks/{position}");
        match block {
            Block::FieldList { of, fields: names } => {
                let doctype = of.as_deref().or(panel.doctype.as_deref());
                if let Some(known) = doctype.and_then(|doctype| cx.facts.doctype(doctype)) {
                    for (index, name) in names.iter().enumerate() {
                        if !known.fields.contains(name) {
                            report(
                                cx,
                                findings,
                                codes::SCHEMA,
                                file,
                                format!("{at}/fields/{index}"),
                                format!("{} has no field {name}", known.name),
                                Some(&panel.id),
                            );
                        }
                    }
                }
            }
            Block::KeyValue { items } => {
                for (index, item) in items.iter().enumerate() {
                    check_expression(
                        findings,
                        cx.app,
                        file,
                        format!("{at}/items/{index}/value"),
                        &item.value,
                        &scope,
                        &what,
                    );
                }
            }
            Block::Text { markdown: source } => match markdown::check(source) {
                Err(markdown::MarkdownError::Markup) => report(
                    cx,
                    findings,
                    codes::RAW_HTML,
                    file,
                    format!("{at}/markdown"),
                    "markup is not allowed".to_owned(),
                    Some(&panel.id),
                ),
                Ok(parsed) => {
                    for destination in parsed.destinations {
                        if !cx.facts.destinations.contains(&destination) {
                            report(
                                cx,
                                findings,
                                codes::UNKNOWN_DESTINATION,
                                file,
                                format!("{at}/markdown"),
                                format!("destination {destination} does not exist"),
                                Some(&panel.id),
                            );
                        }
                    }
                }
            },
            Block::Table { query, columns, .. } => check_data_block(
                cx,
                findings,
                file,
                &at,
                panel,
                query.as_deref(),
                columns,
                QueryKind::Rows,
                "table",
            ),
            Block::Stat { query, .. } => check_data_block(
                cx,
                findings,
                file,
                &at,
                panel,
                query.as_deref(),
                &[],
                QueryKind::Aggregate,
                "stat",
            ),
            Block::ListOfLinks { items } => {
                for (index, item) in items.iter().enumerate() {
                    check_destination(
                        cx,
                        findings,
                        file,
                        &format!("{at}/items/{index}/open"),
                        &item.open,
                        &scope,
                        &what,
                    );
                }
            }
            Block::EmptyState { icon, .. } => {
                if let Some(icon) = icon
                    && !cx.facts.icon_known(icon)
                {
                    report(
                        cx,
                        findings,
                        codes::UNKNOWN_ICON,
                        file,
                        format!("{at}/icon"),
                        format!("icon {icon} is not an icon token"),
                        Some(&panel.id),
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_data_block(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    at: &str,
    panel: &Panel,
    query: Option<&str>,
    columns: &[Column],
    needed: QueryKind,
    block: &str,
) {
    let Some(query) = query else {
        report(
            cx,
            findings,
            codes::QUERY_MISSING,
            file,
            at.to_owned(),
            format!("{at}: a data block names a query"),
            Some(&panel.id),
        );
        return;
    };
    match cx.facts.query(query) {
        None => report(
            cx,
            findings,
            codes::UNKNOWN_QUERY,
            file,
            format!("{at}/query"),
            format!("query {query} is not declared"),
            Some(&panel.id),
        ),
        Some(info) => {
            if info.kind != needed {
                report(
                    cx,
                    findings,
                    codes::QUERY_KIND,
                    file,
                    format!("{at}/query"),
                    format!(
                        "{query} is {}; a {block} needs {}",
                        info.kind.as_str(),
                        needed.as_str()
                    ),
                    Some(&panel.id),
                );
            }
            if !info.columns.is_empty() {
                for (index, column) in columns.iter().enumerate() {
                    if !info.columns.contains(&column.field) {
                        report(
                            cx,
                            findings,
                            codes::SCHEMA,
                            file,
                            format!("{at}/columns/{index}/field"),
                            format!("query {query} has no column {}", column.field),
                            Some(&panel.id),
                        );
                    }
                }
            }
        }
    }
}
