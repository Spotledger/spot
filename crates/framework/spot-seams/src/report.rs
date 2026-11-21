//! The admin override listing (WP-EXT-03): the site's overrides and seams as data.
//!
//! A UI or CLI prints the report; the registry only produces it. The report is derived from
//! the built plans, so it says where each override takes effect, and it never depends on the
//! order the apps were registered in.

use serde::Serialize;
use spot_app_api::OverrideKind;

use crate::explain::Role;
use crate::registry::Registry;

/// What an override does, as the listing shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum OverrideRowKind {
    /// The `by` step runs in the target's place; the target never runs.
    #[serde(rename = "replace")]
    Replace,
    /// The `by` step runs in the target's place and may run the target once.
    #[serde(rename = "wrap")]
    Wrap,
    /// The target never runs.
    #[serde(rename = "disable")]
    Disable,
    /// The `by` step runs immediately before or after the target.
    #[serde(rename = "reorder")]
    Reorder,
}

impl OverrideRowKind {
    fn label(self) -> &'static str {
        match self {
            OverrideRowKind::Replace => "replace",
            OverrideRowKind::Wrap => "wrap",
            OverrideRowKind::Disable => "disable",
            OverrideRowKind::Reorder => "reorder",
        }
    }
}

pub(crate) fn kind_of(k: OverrideKind) -> OverrideRowKind {
    match k {
        OverrideKind::Replace => OverrideRowKind::Replace,
        OverrideKind::Wrap => OverrideRowKind::Wrap,
        OverrideKind::Disable => OverrideRowKind::Disable,
        OverrideKind::Before | OverrideKind::After => OverrideRowKind::Reorder,
    }
}

/// One override installed on the site.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct OverrideRow {
    /// The app that declared the override.
    pub app: String,
    /// What it does.
    pub kind: OverrideRowKind,
    /// The step it changes, as `app:step`.
    pub target_step: String,
    /// The DocType the step belongs to.
    pub doctype: String,
    /// The hook point where it takes effect (the ADR-51 name, e.g. `before_save`).
    pub hook_point: String,
    /// Why, as the declaring app wrote it.
    pub reason: String,
    /// Where the override takes effect in the run order of its hook point: the position of the
    /// plan line the override owns (the `by` step's slot, or the disabled step's slot).
    pub effective_order: usize,
}

/// One offered version of one seam.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct SeamRow {
    /// The seam name, as `app:seam`.
    pub name: String,
    /// The major version.
    pub version: u32,
    /// The app providing the original implementation.
    pub provider_app: String,
    /// The app that replaces it, if the seam was replaced on purpose.
    pub replaced_by: Option<String>,
    /// Why the seam was replaced; empty when it was not.
    pub reason: String,
}

/// Every override and seam installed on a site, as data a UI or CLI can print.
///
/// Field order is stable: the JSON form is part of the listing's contract. The report does not
/// change when apps are registered in a different order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OverrideReport {
    /// Every installed override, sorted by (app, kind, target step, DocType, hook point).
    pub overrides: Vec<OverrideRow>,
    /// Every offered seam version, sorted by (name, version).
    pub seams: Vec<SeamRow>,
}

impl OverrideReport {
    /// A fixed-width table a CLI can print. Column widths follow the content; the text form
    /// carries the same rows as the data form.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let overrides: Vec<Vec<String>> = self
            .overrides
            .iter()
            .map(|r| {
                vec![
                    r.app.clone(),
                    r.kind.label().to_owned(),
                    r.target_step.clone(),
                    r.doctype.clone(),
                    r.hook_point.clone(),
                    r.effective_order.to_string(),
                    r.reason.clone(),
                ]
            })
            .collect();
        table(
            &mut out,
            "OVERRIDES",
            &[
                "app",
                "kind",
                "target_step",
                "doctype",
                "hook_point",
                "order",
                "reason",
            ],
            &overrides,
        );
        let seams: Vec<Vec<String>> = self
            .seams
            .iter()
            .map(|r| {
                vec![
                    r.name.clone(),
                    r.version.to_string(),
                    r.provider_app.clone(),
                    r.replaced_by.clone().unwrap_or_default(),
                    r.reason.clone(),
                ]
            })
            .collect();
        table(
            &mut out,
            "SEAMS",
            &["name", "version", "provider_app", "replaced_by", "reason"],
            &seams,
        );
        out
    }
}

/// Appends one fixed-width table: a title, the header, a dash rule, then the rows.
fn table(out: &mut String, title: &str, headers: &[&str], rows: &[Vec<String>]) {
    let width = headers.len();
    let mut cols: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().take(width).enumerate() {
            cols[i] = cols[i].max(cell.len());
        }
    }
    out.push_str(title);
    out.push('\n');
    line(out, headers, &cols);
    let rule: Vec<String> = cols.iter().map(|w| "-".repeat(*w)).collect();
    line(
        out,
        &rule.iter().map(String::as_str).collect::<Vec<_>>(),
        &cols,
    );
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        line(out, &cells, &cols);
    }
}

/// Appends one padded line of a fixed-width table.
fn line(out: &mut String, cells: &[&str], cols: &[usize]) {
    for (i, cell) in cells.iter().enumerate() {
        out.push_str(cell);
        if i + 1 < cells.len() {
            for _ in cell.len()..cols[i] {
                out.push(' ');
            }
            out.push_str("  ");
        }
    }
    out.push('\n');
}

/// Collects the listing from the built registry.
///
/// Each override owns exactly one line of the built explanations: the line that exists because
/// of it. A `Before`, `After`, `Wrap` or `Replace` owns the line of its `by` step; a `Disable`
/// owns the disabled step's line (`runs == false`). The other mentions of an override (a
/// target's line lists every override that touched it) are not owned lines. `effective_order`
/// is the owned line's position within its (DocType, hook point) plan.
pub(crate) fn collect(registry: &Registry) -> OverrideReport {
    let mut rows: Vec<OverrideRow> = Vec::new();
    let mut doctypes: Vec<&String> = registry.explanations.keys().collect();
    doctypes.sort();
    for doctype in doctypes {
        let points = &registry.explanations[doctype];
        let mut keys: Vec<&spot_app_api::HookPoint> = points.keys().collect();
        keys.sort_by_key(|p| p.name());
        for point in keys {
            let lines = &points[point];
            let name = point.name();
            for (order, line) in lines.iter().enumerate() {
                for o in &line.overrides {
                    let owns = match (&line.role, o.kind) {
                        (Role::Before { target }, OverrideKind::Before)
                        | (Role::After { target }, OverrideKind::After)
                        | (Role::Wrapper { target }, OverrideKind::Wrap)
                        | (Role::Replacement { target }, OverrideKind::Replace) => {
                            target == &o.target
                        }
                        (Role::Declared, OverrideKind::Disable) => {
                            !line.runs && line.step == o.target
                        }
                        _ => false,
                    };
                    if !owns {
                        continue;
                    }
                    rows.push(OverrideRow {
                        app: o.declared_by.clone(),
                        kind: kind_of(o.kind),
                        target_step: o.target.to_string(),
                        doctype: (*doctype).clone(),
                        hook_point: name.clone(),
                        reason: o.reason.clone(),
                        effective_order: order,
                    });
                }
            }
        }
    }
    rows.sort();

    let mut seams = Vec::new();
    let mut names: Vec<&String> = registry.seams.keys().collect();
    names.sort();
    for name in names {
        for (version, e) in &registry.seams[name] {
            seams.push(SeamRow {
                name: (*name).clone(),
                version: *version,
                provider_app: e.app.clone(),
                replaced_by: e.replacement.as_ref().map(|(a, _, _)| a.clone()),
                reason: e
                    .replacement
                    .as_ref()
                    .map_or_else(String::new, |(_, _, r)| r.clone()),
            });
        }
    }
    OverrideReport {
        overrides: rows,
        seams,
    }
}
