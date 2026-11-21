//! The daily check, and the check at the end.
//!
//! Every invariant is named in its message, so a failing day says which one broke and where.
//! The six guarantees of ADR-062 s4 come from [`spot_sync_chaos::check_guarantees`] (the one
//! checker of the suite); the checks here add what only a long mixed run can break.

use std::collections::BTreeMap;

use spot_sync_chaos::{World, check_guarantees};

/// A violated invariant: its name and what was seen. Never record content, only keys and
/// positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The invariant's name.
    pub invariant: &'static str,
    /// What was wrong.
    pub detail: String,
}

impl Violation {
    /// A violation of `invariant`.
    #[must_use]
    pub fn new(invariant: &'static str, detail: impl Into<String>) -> Self {
        Self {
            invariant,
            detail: detail.into(),
        }
    }
}

/// What the run counted about restores, for the daily check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreCount {
    /// Restores the run performed.
    pub restores: usize,
    /// Of those, how many the hub detected as a stale generation and bootstrapped.
    pub bootstrapped: usize,
}

/// The daily check on the world: the six guarantees (all but convergence, which only holds once
/// the world is quiet), tenant isolation, and that every restore was detected and bootstrapped.
///
/// The guarantees cover, by name: per-key order (no version applied out of order at any node,
/// held or not), no loss of an acknowledged record, no record stored twice, no record outside a
/// node's scope except by a recorded closure, and acknowledgements that mean applied.
///
/// # Errors
/// The first violated invariant.
pub fn check_day(world: &World, restores: RestoreCount) -> Result<(), Violation> {
    if let Some(first) = check_guarantees(world, false).into_iter().next() {
        return Err(Violation::new("guarantee", first.to_string()));
    }
    for node in 0..world.nodes() {
        if let Some((tenant, key)) = world.foreign_records(node).into_iter().next() {
            return Err(Violation::new(
                "tenant isolation",
                format!(
                    "node-{node} holds {}/{} of tenant {tenant}",
                    key.entity, key.id
                ),
            ));
        }
    }
    if restores.bootstrapped != restores.restores {
        return Err(Violation::new(
            "every restored node is detected and bootstrapped",
            format!(
                "{} restores, {} detected and bootstrapped: a restored node was continued",
                restores.restores, restores.bootstrapped
            ),
        ));
    }
    Ok(())
}

/// The check after the final convergence phase: every node's applied state equals the state the
/// hub's stream and the node's scope say it must hold.
///
/// The expectation is computed here from the hub's own stream (`World::expected`, the newest
/// version of every record the hub wrote for the tenant) and the node's scope, never from the
/// node's own view of what it was sent.
///
/// # Errors
/// The first node whose state differs, the stream the hub did not finish reading, or a queue that
/// is not empty.
pub fn check_converged(world: &World) -> Result<(), Violation> {
    if world.cursor() != world.head() {
        return Err(Violation::new(
            "final convergence",
            format!(
                "the hub read to {} but the stream is at {}",
                world.cursor(),
                world.head()
            ),
        ));
    }
    let expected = world.expected();
    for node in 0..world.nodes() {
        let Some(scope) = world.scope(node) else {
            return Err(Violation::new(
                "final convergence",
                format!("no node-{node}"),
            ));
        };
        let wanted: BTreeMap<_, _> = expected
            .iter()
            .filter(|(key, _)| scope.covers_entity(&key.entity))
            .map(|(key, version)| (key.clone(), *version))
            .collect();
        let held = world.applied(node);
        if held != wanted {
            let missing = wanted.keys().filter(|key| !held.contains_key(*key)).count();
            let stale = wanted
                .iter()
                .filter(|(key, version)| held.get(*key).is_some_and(|have| have < *version))
                .count();
            let extra = held.keys().filter(|key| !wanted.contains_key(*key)).count();
            return Err(Violation::new(
                "final convergence",
                format!(
                    "node-{node} holds {} records, its scope says {}: {missing} missing, {stale} \
                     behind, {extra} it should not hold",
                    held.len(),
                    wanted.len()
                ),
            ));
        }
        if world.queued(node) != 0 {
            return Err(Violation::new(
                "final convergence",
                format!(
                    "node-{node} still has {} bundles queued",
                    world.queued(node)
                ),
            ));
        }
    }
    if let Some(first) = check_guarantees(world, true).into_iter().next() {
        return Err(Violation::new("guarantee", first.to_string()));
    }
    Ok(())
}
