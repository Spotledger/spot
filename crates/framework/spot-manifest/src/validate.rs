//! The shared checks every manifest must pass, whether parsed from TOML or built in code.
//!
//! These stay single-manifest: installing one app needs no other manifest around, so the
//! parser and code-built manifests run the same rules. Cross-app checks — a seam is still
//! provided somewhere, an override target exists, a foreign step really runs in its app —
//! are the installer's work, which sees every manifest together.

use std::collections::HashSet;
use std::time::Duration;

use spot_app_api::{OverrideKind, StepName};

use crate::error::{
    BUDGET_OUT_OF_RANGE, DUPLICATE_PATCH, DUPLICATE_SCHEDULE, DUPLICATE_STEP, FOREIGN_STEP,
    ManifestError, OVERRIDE_MISSING_BY, OVERRIDE_WITH_BY, PATCH_ORDER, PERMISSION_DUPLICATE,
    PERMISSION_TITLE,
};
use crate::model::{
    AppManifest, EVENT_BUDGET_LIMIT_MS, MAX_PERMISSION_TITLE_CHARS, SCHEDULE_BUDGET_LIMIT_MS,
};

/// Where a `[[permissions]]` row sits in the source: its `name` and its `title`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PermPos {
    /// Line and column of the row's `name` value, when known.
    pub name: Option<(u32, u32)>,
    /// Line and column of the row's `title` value, when known.
    pub title: Option<(u32, u32)>,
}

/// Where a row sits in the source: its anchor item and, when there is one, its budget value.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pos {
    /// Line and column of the row's anchor item, when known.
    pub anchor: Option<(u32, u32)>,
    /// Line and column of the row's `budget_ms` value, when there is one.
    pub budget: Option<(u32, u32)>,
}

/// The row positions the parser collected, so shared checks can point at a line.
#[derive(Debug, Default)]
pub(crate) struct Spans {
    /// The `[[events]]` rows, in order.
    pub events: Vec<Pos>,
    /// The `[[session_events]]` rows, in order.
    pub session: Vec<Pos>,
    /// The `[[boot]]` rows, in order.
    pub boot: Vec<Pos>,
    /// The `[[schedules]]` rows, in order.
    pub schedules: Vec<Pos>,
    /// The `[[overrides]]` rows, in order.
    pub overrides: Vec<Pos>,
    /// The `[[fixtures]]` rows, in order.
    pub fixtures: Vec<Pos>,
    /// The `[[patches]]` rows, in order.
    pub patches: Vec<Pos>,
    /// The `[[routes]]` rows, in order.
    pub routes: Vec<Pos>,
    /// The `[[permissions]]` rows, in order.
    pub permissions: Vec<PermPos>,
}

/// Validates a manifest built in code, running the same rules as for parsed manifests.
pub fn validate(manifest: &AppManifest) -> Vec<ManifestError> {
    check_rules(manifest, None)
}

/// The recorded position of row `i`, or none when there is none to record. Rows that
/// failed to read are dropped but may still have pushed a span, so rows and spans can
/// disagree in length; a missing position only means the error has no line and column.
fn row_at(pos: &[Pos], i: usize) -> Pos {
    pos.get(i).copied().unwrap_or_default()
}

/// The shared checks for one manifest. `spans` carries row positions when the manifest was
/// parsed, so errors can point at a line and column.
pub(crate) fn check_rules(manifest: &AppManifest, spans: Option<&Spans>) -> Vec<ManifestError> {
    const NO_POS: &[Pos] = &[];
    let mut errs = Vec::new();
    let app = manifest.app.name.as_str();

    // A step subscribes once per doctype and hook point.
    let mut fired = HashSet::new();
    let pos = spans.map_or(NO_POS, |sp| sp.events.as_slice());
    for (i, sub) in manifest.events.iter().enumerate() {
        let row = &row_at(pos, i);
        if sub.step.app() != app {
            errs.push(with_pos(foreign_step(&sub.step, app, "[[events]]"), row));
        }
        check_budget(
            &mut errs,
            sub.budget,
            EVENT_BUDGET_LIMIT_MS,
            "[[events]] budget",
            row,
        );
        let key = format!("{}:{}:{}", sub.doctype, sub.event.name(), sub.step);
        if !fired.insert(key) {
            errs.push(with_pos(
                ManifestError::new(
                    DUPLICATE_STEP,
                    format!(
                        "[[events]]: the step `{}` is subscribed to the same doctype and hook point twice",
                        sub.step
                    ),
                ),
                row,
            ));
        }
    }

    // A step subscribes to a session event once.
    let mut fired = HashSet::new();
    let pos = spans.map_or(NO_POS, |sp| sp.session.as_slice());
    for (i, sub) in manifest.session_events.iter().enumerate() {
        let row = &row_at(pos, i);
        if sub.step.app() != app {
            errs.push(with_pos(
                foreign_step(&sub.step, app, "[[session_events]]"),
                row,
            ));
        }
        check_budget(
            &mut errs,
            sub.budget,
            EVENT_BUDGET_LIMIT_MS,
            "[[session_events]] budget",
            row,
        );
        let key = format!("{:?}:{}", sub.event, sub.step);
        if !fired.insert(key) {
            errs.push(with_pos(
                ManifestError::new(
                    DUPLICATE_STEP,
                    format!(
                        "[[session_events]]: the step `{}` is subscribed to the same event twice",
                        sub.step
                    ),
                ),
                row,
            ));
        }
    }

    let pos = spans.map_or(NO_POS, |sp| sp.boot.as_slice());
    for (i, sub) in manifest.boot.iter().enumerate() {
        let row = &row_at(pos, i);
        if sub.step.app() != app {
            errs.push(with_pos(foreign_step(&sub.step, app, "[[boot]]"), row));
        }
        if let Some(budget) = sub.budget {
            check_budget(
                &mut errs,
                budget,
                EVENT_BUDGET_LIMIT_MS,
                "[[boot]] budget",
                row,
            );
        }
    }

    // Schedule names are unique in the manifest.
    let mut names = HashSet::new();
    let pos = spans.map_or(NO_POS, |sp| sp.schedules.as_slice());
    for (i, sub) in manifest.schedules.iter().enumerate() {
        let row = &row_at(pos, i);
        if sub.step.app() != app {
            errs.push(with_pos(foreign_step(&sub.step, app, "[[schedules]]"), row));
        }
        check_budget(
            &mut errs,
            sub.budget,
            SCHEDULE_BUDGET_LIMIT_MS,
            "[[schedules]] budget",
            row,
        );
        if !names.insert(sub.name.as_str()) {
            errs.push(with_pos(
                ManifestError::new(
                    DUPLICATE_SCHEDULE,
                    format!("[[schedules]]: the name `{}` is used twice", sub.name),
                ),
                row,
            ));
        }
    }

    // Patch ids are unique and sort in the order they are listed.
    let mut seen = HashSet::new();
    let mut last: Option<&str> = None;
    let pos = spans.map_or(NO_POS, |sp| sp.patches.as_slice());
    for (i, sub) in manifest.patches.iter().enumerate() {
        let row = &row_at(pos, i);
        if !seen.insert(sub.id.as_str()) {
            errs.push(with_pos(
                ManifestError::new(
                    DUPLICATE_PATCH,
                    format!("[[patches]]: the id `{}` is listed twice", sub.id),
                ),
                row,
            ));
        }
        if let Some(prev) = last
            && sub.id.as_str() < prev
        {
            errs.push(with_pos(
                ManifestError::new(
                    PATCH_ORDER,
                    format!(
                        "[[patches]]: the id `{}` sorts before `{}`, the id listed before it; \
                         list patch ids in ascending order",
                        sub.id, prev
                    ),
                ),
                row,
            ));
        }
        last = Some(sub.id.as_str());
    }

    // `by` runs a step of this app; only `disable` may leave it out.
    let pos = spans.map_or(NO_POS, |sp| sp.overrides.as_slice());
    for (i, sub) in manifest.overrides.iter().enumerate() {
        let row = &row_at(pos, i);
        match &sub.by {
            Some(by) => {
                if by.app() != app {
                    errs.push(with_pos(foreign_step(by, app, "[[overrides]] `by`"), row));
                }
                if sub.kind == OverrideKind::Disable {
                    errs.push(with_pos(
                        ManifestError::new(
                            OVERRIDE_WITH_BY,
                            format!(
                                "[[overrides]]: `by = {by}` is meaningless with `kind = {:?}`: a \
                                 disable turns the target off, it runs no step",
                                sub.kind
                            ),
                        ),
                        row,
                    ));
                }
            }
            None => {
                if sub.kind != OverrideKind::Disable {
                    errs.push(with_pos(
                        ManifestError::new(
                            OVERRIDE_MISSING_BY,
                            format!(
                                "[[overrides]]: a `kind = {:?}` override needs a `by` step; only \
                                 `disable` runs nothing",
                                sub.kind
                            ),
                        ),
                        row,
                    ));
                }
            }
        }
    }

    let pos = spans.map_or(NO_POS, |sp| sp.routes.as_slice());
    for (i, sub) in manifest.routes.iter().enumerate() {
        if sub.step.app() != app {
            errs.push(with_pos(
                foreign_step(&sub.step, app, "[[routes]]"),
                &row_at(pos, i),
            ));
        }
    }

    let pos = spans.map_or(NO_POS, |sp| sp.fixtures.as_slice());
    for (i, sub) in manifest.fixtures.iter().enumerate() {
        if let Some(step) = &sub.step
            && step.app() != app
        {
            errs.push(with_pos(
                foreign_step(step, app, "[[fixtures]] `step`"),
                &row_at(pos, i),
            ));
        }
    }

    // Permission titles are non-empty and short; names are unique in the manifest. Cross-app
    // uniqueness is a site fact, checked when the catalogue is built (`insert`), not here.
    let mut declared = HashSet::new();
    let pos = spans.map_or(&[][..], |sp| sp.permissions.as_slice());
    for (i, sub) in manifest.permissions.iter().enumerate() {
        let row = pos.get(i).copied().unwrap_or_default();
        if sub.title.trim().is_empty() {
            errs.push(at(
                ManifestError::new(
                    PERMISSION_TITLE,
                    format!(
                        "[[permissions]]: the permission `{}` needs a `title` for the role editor",
                        sub.name
                    ),
                ),
                row.title.or(row.name),
            ));
        } else {
            let len = sub.title.chars().count();
            if len > MAX_PERMISSION_TITLE_CHARS {
                errs.push(at(
                    ManifestError::new(
                        PERMISSION_TITLE,
                        format!(
                            "[[permissions]]: the title of `{}` is {len} characters, over the \
                             {MAX_PERMISSION_TITLE_CHARS}-character limit",
                            sub.name
                        ),
                    ),
                    row.title.or(row.name),
                ));
            }
        }
        if !declared.insert(sub.name.as_str()) {
            errs.push(at(
                ManifestError::new(
                    PERMISSION_DUPLICATE,
                    format!(
                        "[[permissions]]: the name `{}` is declared twice; cross-app uniqueness \
                         is checked when the site's catalogue is built",
                        sub.name
                    ),
                ),
                row.name,
            ));
        }
    }

    errs
}

/// A foreign step: every `step` in the manifest belongs to this app. Only an override
/// `target` may name another app's step.
fn foreign_step(step: &StepName, app: &str, what: &str) -> ManifestError {
    ManifestError::new(
        FOREIGN_STEP,
        format!(
            "{what}: the step `{step}` belongs to app `{}`, but every `step` in the manifest \
             must belong to this app (`{app}`); only an override `target` may name another app",
            step.app()
        ),
    )
}

/// A budget must be at least 1 ms and at most `limit` ms.
fn check_budget(
    errs: &mut Vec<ManifestError>,
    budget: Duration,
    limit: u64,
    what: &str,
    row: &Pos,
) {
    let ms = budget.as_millis();
    if ms == 0 || ms > u128::from(limit) {
        errs.push(with_budget_pos(
            ManifestError::new(
                BUDGET_OUT_OF_RANGE,
                format!("{what} budget {ms} ms must be at least 1 ms and at most {limit} ms"),
            ),
            row,
        ));
    }
}

/// Attaches a position, when the parse recorded one.
fn at(err: ManifestError, pos: Option<(u32, u32)>) -> ManifestError {
    match pos {
        Some((line, col)) => err.at(line, col),
        None => err,
    }
}

/// Attaches the row's anchor position when the parse recorded one.
fn with_pos(err: ManifestError, row: &Pos) -> ManifestError {
    at(err, row.anchor)
}

/// Attaches the budget value's position, falling back to the row anchor.
fn with_budget_pos(err: ManifestError, row: &Pos) -> ManifestError {
    match row.budget.or(row.anchor) {
        Some((line, col)) => err.at(line, col),
        None => err,
    }
}
