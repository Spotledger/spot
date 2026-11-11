//! The patch diff: which patches run, in which order, and which are missing.
//!
//! A patch in the target system that the applied list does not name is planned, in dependency
//! order (a patch's `deps` first, ties broken by id). A patch with no declared intent is
//! [`crate::risk::Risk::NeedsReview`]: the strict "every patch declares its intent" rule is
//! WP-MIG-02's, but the plan flags it now. A patch in the applied list that the target system
//! does not name is a downgrade and is reported.

use std::collections::BTreeSet;

use crate::change::{BlockingClass, Change, ChangeKind};
use crate::model::{PatchDecl, SystemDescription};
use crate::risk::{RiskTable, rules};

/// Diffs the patches of `from` against `to`, using `to`'s applied list as the live state.
pub fn diff(from: &SystemDescription, to: &SystemDescription, table: &RiskTable) -> Vec<Change> {
    let _ = from;
    let applied: BTreeSet<&str> = to.applied_patches.iter().map(String::as_str).collect();
    let declared: Vec<&PatchDecl> = to.patches();
    let declared_ids: BTreeSet<&str> = declared.iter().map(|p| p.id.as_str()).collect();

    let mut out = Vec::new();
    for patch in dependency_order(&declared) {
        if applied.contains(patch.id.as_str()) {
            continue;
        }
        out.push(patch_added(patch, table));
    }
    for id in &to.applied_patches {
        if !declared_ids.contains(id.as_str()) {
            out.push(patch_missing(id, table));
        }
    }
    out
}

/// A patch that will run.
fn patch_added(patch: &PatchDecl, table: &RiskTable) -> Change {
    let (rule, why) = match &patch.intent {
        Some(_) => (rules::PATCH_ADDED, None),
        None => (
            rules::PATCH_UNDECIDED,
            Some("the patch declares no intent; declare it as schema, data or metadata"),
        ),
    };
    let mut c = Change::new(
        format!("patch/{}", patch.id),
        ChangeKind::PatchAdded,
        rule,
        patch.id.clone(),
        format!(
            "Run patch `{}`{}.",
            patch.id,
            match &patch.intent {
                Some(intent) => format!(" (intent: {intent})"),
                None => String::new(),
            }
        ),
        table.risk_of(rule),
        BlockingClass::BriefLock,
    )
    .evidence("not applied", "applied");
    if let Some(why) = why {
        c = c.why(why);
    }
    c
}

/// A patch that is applied but absent from the new system.
fn patch_missing(id: &str, table: &RiskTable) -> Change {
    let rule = rules::PATCH_MISSING;
    Change::new(
        format!("patch/{id}/missing"),
        ChangeKind::PatchMissing,
        rule,
        id.to_owned(),
        format!("Patch `{id}` is applied but absent from the new system (a downgrade)."),
        table.risk_of(rule),
        BlockingClass::MaintenanceWindow,
    )
    .evidence("applied", "absent")
    .why("a downgrade must be requested explicitly and may need a down-step")
    .alternative("keep the patch in the new system, or provide a down-step before removing it")
}

/// Orders patches so every dependency runs first, ties broken by id, cycles by id.
fn dependency_order<'a>(patches: &[&'a PatchDecl]) -> Vec<&'a PatchDecl> {
    let mut remaining: Vec<&PatchDecl> = patches.to_vec();
    remaining.sort_by(|a, b| a.id.cmp(&b.id));
    let mut emitted: Vec<&PatchDecl> = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    while !remaining.is_empty() {
        let mut progress = false;
        let mut i = 0;
        while i < remaining.len() {
            let ready = remaining[i]
                .deps
                .iter()
                .all(|d| done.contains(d) || !remaining.iter().any(|q| &q.id == d));
            if ready {
                let patch = remaining.remove(i);
                done.insert(patch.id.clone());
                emitted.push(patch);
                progress = true;
            } else {
                i += 1;
            }
        }
        if !progress {
            for patch in remaining.drain(..) {
                done.insert(patch.id.clone());
                emitted.push(patch);
            }
        }
    }
    emitted
}
