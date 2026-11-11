//! The hook the metadata registry build and the app installer call.
//!
//! `spot-meta` builds the effective metadata; `spot-installer` installs an app. Both must run the
//! policy lints **before** the metadata goes live (ADR-062 rule 3). This crate provides the one
//! function each calls — [`verify`] on a save or an install, [`verify_change`] when records may
//! already be queued — and returns **every** problem at once so the author sees all of them, not
//! the first. Editing `spot-meta` is out of this task's scope; the seam is this public function.

use spot_meta::EffectiveMeta;

use crate::error::codes;
use crate::lint::{PolicyProblem, QueuedRecords, lint, lint_change};
use crate::table::{PolicySet, PolicyTable, resolve};

/// Resolves the policies over `meta` and lints them. `Ok` carries the [`PolicyTable`] when the
/// metadata is consistent; `Err` carries every problem at once.
///
/// # Errors
///
/// A `Vec<PolicyProblem>`: one per declaration naming an unknown DocType, then every
/// [`lint`] problem.
pub fn verify(
    meta: &EffectiveMeta,
    declared: &PolicySet,
) -> Result<PolicyTable, Vec<PolicyProblem>> {
    let table = resolve(meta, declared);
    let mut problems = unknown_doctype_problems(&table);
    problems.extend(lint(&table, meta));
    finish(table, problems)
}

/// As [`verify`], but also checks a policy change against records already queued for delivery
/// ([`lint_change`]).
///
/// # Errors
///
/// A `Vec<PolicyProblem>`: the [`verify`] problems plus one per DocType whose change would strand
/// queued records.
pub fn verify_change(
    meta: &EffectiveMeta,
    declared: &PolicySet,
    previous: &PolicyTable,
    queued: &QueuedRecords,
) -> Result<PolicyTable, Vec<PolicyProblem>> {
    let table = resolve(meta, declared);
    let mut problems = unknown_doctype_problems(&table);
    problems.extend(lint(&table, meta));
    problems.extend(lint_change(previous, &table, queued));
    finish(table, problems)
}

/// One `sync_policy.unknown_doctype` problem per declared DocType the metadata does not hold.
pub(crate) fn unknown_doctype_problems(table: &PolicyTable) -> Vec<PolicyProblem> {
    table
        .unknown()
        .iter()
        .map(|name| {
            PolicyProblem::new(
                codes::UNKNOWN_DOCTYPE,
                name,
                "a policy is declared for a DocType the metadata does not hold",
            )
        })
        .collect()
}

/// The shared tail: hand back the table, or every problem.
fn finish(
    table: PolicyTable,
    problems: Vec<PolicyProblem>,
) -> Result<PolicyTable, Vec<PolicyProblem>> {
    if problems.is_empty() {
        Ok(table)
    } else {
        Err(problems)
    }
}
