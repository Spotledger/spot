//! `spot-sync-health`: the sync health surface as a model (WP-SYN-10.T1, ADR-067 section 6).
//!
//! [`build`] turns the open conditions and the nodes' facts into a [`HealthView`] the Admin
//! Center renders: a [`Status`] (`Calm` by default), one headline key, the short `needs you`
//! list, per-node rows with their lag, and counts. No UI, no notification and no route wiring
//! here. A pure library: no clock (`now` is an argument), no I/O, no randomness.
//!
//! # What reaches `needs you`
//!
//! Only a condition whose definition says a person must respond and whose automatic response
//! is exhausted or never applied (see [`needs_a_person`]). Everything else is counted in
//! [`Counts`], not listed: a thousand `info` conditions are `Calm` with an empty list.
//! Items with the same remedy collapse into one row with a count and up to
//! [`SAMPLE_SIZE`] subjects (`+N more`); rows are sorted by severity, then oldest first, then
//! by key, so the order never changes between two calls at the same instant.
//!
//! # Tenancy and permission
//!
//! The view is **per tenant**: the caller passes only that tenant's open conditions and nodes,
//! and the model has no way to pull in anything else. Subjects and parameters are shown only
//! when they look like ids; anything else is replaced by [`REDACTED`], so a payload cannot
//! reach the screen through a subject. **The permission to see this view is the caller's:** the
//! API task must check the admin permission before it calls [`build`] or serves the JSON.
//!
//! # Time
//!
//! Every time is hub time in milliseconds. The hub's view of a node is **as-of** its last
//! report (ADR-065 section 8): each [`NodeRow`] carries its own `as_of`.

mod error;
mod facts;
mod model;
mod needs;

use std::collections::{BTreeMap, BTreeSet};

use spot_conditions::{HealthThresholds, Millis, Open, Severity, SeverityCounts};

pub use error::{HealthError, NEGATIVE_CONTACT_TIME};
pub use facts::NodeHealthFacts;
pub use model::{
    Colour, Counts, HealthView, MAX_ROWS, NeedsYou, NodeRow, NodeStatus, Route, SAMPLE_SIZE, Status,
};
pub use needs::{REDACTED, REVIEW_REMEDY, needs_a_person};

/// Build the health view of one tenant at `now`.
#[must_use]
pub fn build(
    open: &[Open],
    nodes: &[NodeHealthFacts],
    now: Millis,
    thresholds: &HealthThresholds,
) -> HealthView {
    let catalogue: BTreeMap<String, spot_sync_conditions::CatalogueEntry> =
        spot_sync_conditions::entries()
            .into_iter()
            .map(|entry| (entry.def.id.clone(), entry))
            .collect();

    let listed: Vec<(&Open, Option<&spot_sync_conditions::CatalogueEntry>)> = open
        .iter()
        .map(|condition| (condition, catalogue.get(condition.id.as_str())))
        .filter(|(condition, entry)| needs_a_person(condition, *entry, now, thresholds))
        .collect();

    let mut needs_you = needs::group(&listed);
    let needs_you_rows = needs_you.len();
    let mut truncated = needs::cap(&mut needs_you);

    let status = if listed
        .iter()
        .any(|(condition, _)| condition.severity == Severity::Red)
    {
        Status::Action
    } else if listed.is_empty() {
        Status::Calm
    } else {
        Status::Attention
    };

    let mut rows = node_rows(open, nodes, now);
    let node_count = rows.len();
    if rows.len() > MAX_ROWS {
        rows.truncate(MAX_ROWS);
        truncated = true;
    }

    let severities = SeverityCounts::of(open);
    HealthView {
        status,
        headline_key: status.headline_key().to_owned(),
        now,
        needs_you,
        nodes: rows,
        counts: Counts {
            info: severities.info,
            amber: severities.amber,
            red: severities.red,
            needs_you_items: listed.len(),
            needs_you_rows,
            nodes: node_count,
        },
        truncated,
    }
}

/// The node rows, worst first, then by node id. A node given twice keeps its newest facts.
fn node_rows(open: &[Open], nodes: &[NodeHealthFacts], now: Millis) -> Vec<NodeRow> {
    let mut newest: BTreeMap<&str, &NodeHealthFacts> = BTreeMap::new();
    for facts in nodes {
        newest
            .entry(facts.node.as_str())
            .and_modify(|kept| {
                if facts.as_of > kept.as_of {
                    *kept = facts;
                }
            })
            .or_insert(facts);
    }
    // Conditions on `node:<id>` (the subject the ledger uses) or on the bare id.
    let mut worst: BTreeMap<&str, Severity> = BTreeMap::new();
    let known: BTreeSet<&str> = newest.keys().copied().collect();
    for condition in open {
        let subject = condition.subject.as_str();
        let id = subject.strip_prefix("node:").unwrap_or(subject);
        if known.contains(id) {
            worst
                .entry(id)
                .and_modify(|severity| *severity = (*severity).max(condition.severity))
                .or_insert(condition.severity);
        }
    }
    let mut rows: Vec<NodeRow> = newest
        .into_values()
        .map(|facts| {
            let from_conditions = match worst.get(facts.node.as_str()) {
                Some(Severity::Red) => Colour::Red,
                Some(Severity::Amber) => Colour::Amber,
                Some(Severity::Info) | None => Colour::Green,
            };
            NodeRow {
                node: facts.node.clone(),
                last_contact: facts.last_contact,
                silent_for: now.since(facts.last_contact),
                lag_positions: facts.lag_positions(),
                lag_estimate: facts.lag_estimate(),
                version: facts.version.clone(),
                status: facts.status,
                colour: facts.status.colour().max(from_conditions),
                as_of: facts.as_of,
            }
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .colour
            .cmp(&left.colour)
            .then_with(|| left.node.cmp(&right.node))
    });
    rows
}
