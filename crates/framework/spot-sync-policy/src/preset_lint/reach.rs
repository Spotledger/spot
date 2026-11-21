//! Lint L7: reference reach.
//!
//! A `master` reaches the nodes its `scope` selects. Everything it references must reach those
//! nodes too, or a destination could receive a master it cannot complete. The reach of an
//! entity is a set of selectors; two reaches are compared conservatively:
//!
//! * a target scoped `all` covers anything;
//! * a referrer scoped `all` is not covered by anything narrower: a **gap**;
//! * the same `group` tag covers the same `group` tag (a tag names one set of nodes);
//! * two different groups may overlap, and a `key` selector reads a value from a record whose
//!   relation to another record cannot be known here: **undecided**, and reported;
//! * a target with no scope reaches nobody: a **gap**.
//!
//! A link the author marks as carried by reference (ADR-064 extras) is not compared: the target
//! arrives at the referrer's nodes by the closure. Its referrer's nodes then become part of the
//! target's **audience**, which the target's own links must cover in turn, so a chain of masters
//! with different scopes is checked end to end.

use std::collections::BTreeMap;

use super::links::{Edge, edges_of};
use super::{Ctx, Finding, codes};
use crate::preset::{Preset, Selector};

type Reach = Vec<Selector>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Covered,
    Undecided,
    Gap,
}

fn insert(reach: &mut Reach, selector: &Selector) -> bool {
    if reach.contains(selector) {
        false
    } else {
        reach.push(selector.clone());
        true
    }
}

fn describe(reach: &Reach) -> String {
    if reach.is_empty() {
        return "no node".to_owned();
    }
    reach
        .iter()
        .map(|s| match s {
            Selector::All => "all".to_owned(),
            Selector::Group(tag) => format!("group {tag}"),
            Selector::Key { field, key_type } => format!("key {key_type} from {field}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `have` covers one selector `need`.
fn covers(have: &Reach, need: &Selector) -> Verdict {
    if have.contains(&Selector::All) {
        return Verdict::Covered;
    }
    if have.is_empty() || *need == Selector::All {
        return Verdict::Gap;
    }
    if matches!(need, Selector::Group(_)) && have.contains(need) {
        return Verdict::Covered;
    }
    Verdict::Undecided
}

fn compare(have: &Reach, need: &Reach) -> Verdict {
    need.iter()
        .map(|selector| covers(have, selector))
        .max()
        .unwrap_or(Verdict::Covered)
}

fn own_reach(ctx: &Ctx<'_>, doctype: &str) -> Reach {
    let mut reach = Reach::new();
    if let Some(selector) = ctx.table.get(doctype).and_then(|p| p.scope.as_ref()) {
        insert(&mut reach, selector);
    }
    reach
}

/// Whether `preset` delivers records down to nodes by its scope (so reach is meaningful).
fn goes_down(preset: Preset) -> bool {
    matches!(preset, Preset::Master | Preset::Handover)
}

pub(crate) fn lint_reach(ctx: &Ctx<'_>, out: &mut Vec<Finding>) {
    let masters: Vec<&str> = ctx
        .roots()
        .filter(|d| ctx.preset(&d.name) == Preset::Master)
        .map(|d| d.name.as_str())
        .collect();
    if masters.is_empty() {
        return;
    }
    let edges: BTreeMap<&str, Vec<Edge>> = masters
        .iter()
        .filter_map(|name| ctx.defs.get(name).map(|d| (*name, edges_of(ctx, d))))
        .collect();
    let mut audience: BTreeMap<&str, Reach> = masters
        .iter()
        .map(|name| (*name, own_reach(ctx, name)))
        .collect();
    // A reference-carried link brings the referrer's nodes into the target's audience. The
    // selectors are finite and only ever added, so this reaches a fixed point.
    let mut changed = true;
    while changed {
        changed = false;
        for (referrer, list) in &edges {
            let from = audience.get(referrer).cloned().unwrap_or_default();
            for edge in list.iter().filter(|e| e.carried) {
                for target in &edge.targets {
                    if let Some(reach) = audience.get_mut(target.as_str()) {
                        for selector in &from {
                            changed |= insert(reach, selector);
                        }
                    }
                }
            }
        }
    }
    for (referrer, list) in &edges {
        let need = audience.get(referrer).cloned().unwrap_or_default();
        for edge in list.iter().filter(|e| !e.carried) {
            for target in &edge.targets {
                let Some(target_def) = ctx.defs.get(target.as_str()) else {
                    continue;
                };
                if target_def.is_child || !goes_down(ctx.preset(target)) {
                    // Local, record and child targets are L1 and L2 findings.
                    continue;
                }
                let have = own_reach(ctx, target);
                let (code, how) = match compare(&have, &need) {
                    Verdict::Covered => continue,
                    Verdict::Gap => (codes::LINT_REACH_GAP, "does not reach"),
                    Verdict::Undecided => (codes::LINT_REACH_UNDECIDED, "cannot be shown to reach"),
                };
                out.push(Finding::new(
                    code,
                    referrer,
                    format!(
                        "`{referrer}` link `{}` points to `{target}`, whose scope ({}) {how} \
                         every node the referrer's audience ({}) reaches; widen the target's \
                         scope, or mark the link as carried by reference",
                        edge.path,
                        describe(&have),
                        describe(&need)
                    ),
                    &[target],
                ));
            }
        }
    }
}
