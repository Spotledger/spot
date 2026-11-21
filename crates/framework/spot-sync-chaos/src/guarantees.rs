//! The one checker every scenario calls.

use std::collections::BTreeMap;
use std::fmt;

use crate::seed::replay_line;
use crate::world::World;

/// The six properties the suite is judged by (ADR-062 section 4, named the way the series
/// states them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Guarantee {
    /// Every node's state equals the hub's for its scope and nothing is left in flight.
    Convergence,
    /// A node applies the versions of one record in the order the hub wrote them.
    PerKeyOrder,
    /// Everything at or below a prefix a node acknowledged is still held by that node.
    NoLossOfAcknowledged,
    /// No record version was stored twice: duplicates and replays have no effect.
    Idempotence,
    /// No node holds a record of another tenant or outside its scope.
    ScopeIsolation,
    /// Every acknowledgement was sent when everything it claimed was applied.
    AcknowledgementsMeanApplied,
}

impl Guarantee {
    /// The six, in order.
    #[must_use]
    pub fn all() -> [Self; 6] {
        [
            Self::Convergence,
            Self::PerKeyOrder,
            Self::NoLossOfAcknowledged,
            Self::Idempotence,
            Self::ScopeIsolation,
            Self::AcknowledgementsMeanApplied,
        ]
    }

    /// The guarantee's number in the checker's list.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::Convergence => 1,
            Self::PerKeyOrder => 2,
            Self::NoLossOfAcknowledged => 3,
            Self::Idempotence => 4,
            Self::ScopeIsolation => 5,
            Self::AcknowledgementsMeanApplied => 6,
        }
    }
}

/// One guarantee that did not hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Which guarantee.
    pub guarantee: Guarantee,
    /// The node it concerns, when it concerns one.
    pub node: Option<usize>,
    /// What was wrong, in words.
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.node {
            Some(node) => write!(
                f,
                "guarantee {} ({:?}) broken at node-{node}: {}",
                self.guarantee.number(),
                self.guarantee,
                self.detail
            ),
            None => write!(
                f,
                "guarantee {} ({:?}) broken: {}",
                self.guarantee.number(),
                self.guarantee,
                self.detail
            ),
        }
    }
}

/// Check the six guarantees; every violation, in guarantee order.
///
/// Call it after the world has settled (it asserts convergence), or after any step to check the
/// five that must hold at every moment: pass `settled = false` to skip convergence.
#[must_use]
pub fn check_guarantees(world: &World, settled: bool) -> Vec<Violation> {
    let mut found = Vec::new();
    let mut note = |guarantee, node, detail: String| {
        found.push(Violation {
            guarantee,
            node,
            detail,
        });
    };
    if settled && !world.converged() {
        for node in 0..world.nodes() {
            if !world.state_matches(node) {
                note(
                    Guarantee::Convergence,
                    Some(node),
                    format!(
                        "holds {} records, the hub's scope view has {}",
                        world.applied(node).len(),
                        world
                            .expected()
                            .keys()
                            .filter(|k| world
                                .scope(node)
                                .is_some_and(|s| s.covers_entity(&k.entity)))
                            .count()
                    ),
                );
            }
        }
        if world.cursor() != world.head() {
            note(
                Guarantee::Convergence,
                None,
                format!(
                    "the hub read to {} but the stream is at {}",
                    world.cursor(),
                    world.head()
                ),
            );
        }
    }
    for node in 0..world.nodes() {
        let Some(store) = world.store(node) else {
            continue;
        };
        // 2. Per-key order: within one record the versions a node stored only ever rise.
        let mut last: BTreeMap<(&str, &spot_sync_closure::RecordKey), u64> = BTreeMap::new();
        for (tenant, key, version) in store.history() {
            let slot = last.entry((tenant.as_str(), key)).or_insert(0);
            if *version <= *slot {
                note(
                    Guarantee::PerKeyOrder,
                    Some(node),
                    format!("{}/{} stored v{version} after v{slot}", key.entity, key.id),
                );
            }
            *slot = (*slot).max(*version);
        }
        // 3. No loss of acknowledged records.
        let acked = world
            .acks()
            .iter()
            .filter(|ack| ack.node == node)
            .map(|ack| ack.prefix)
            .max();
        if let Some(prefix) = acked {
            let missing = world.missing_through(node, prefix);
            if !missing.is_empty() {
                note(
                    Guarantee::NoLossOfAcknowledged,
                    Some(node),
                    format!(
                        "acknowledged through {prefix} but no longer holds positions {missing:?}"
                    ),
                );
            }
        }
        // 4. Idempotence.
        for (key, version) in store.repeated() {
            note(
                Guarantee::Idempotence,
                Some(node),
                format!(
                    "{}/{} v{version} was stored more than once",
                    key.entity, key.id
                ),
            );
        }
        // 5. Scope isolation: no other tenant, nothing outside the node's entities.
        for (tenant, key) in world.foreign_records(node) {
            note(
                Guarantee::ScopeIsolation,
                Some(node),
                format!("holds {}/{} of tenant {tenant}", key.entity, key.id),
            );
        }
        let extras = world.extras(node);
        for key in store.records_of(world.tenant()).keys() {
            let covered = world
                .scope(node)
                .is_some_and(|scope| scope.covers_entity(&key.entity));
            if !covered && !extras.contains(key) {
                note(
                    Guarantee::ScopeIsolation,
                    Some(node),
                    format!("holds {}/{} outside its scope", key.entity, key.id),
                );
            }
        }
    }
    // 6. Acknowledgements mean applied: judged at the moment each was sent.
    for ack in world.acks() {
        if !ack.missing.is_empty() {
            note(
                Guarantee::AcknowledgementsMeanApplied,
                Some(ack.node),
                format!(
                    "round {} acknowledged prefix {} while positions {:?} were not applied",
                    ack.round, ack.prefix, ack.missing
                ),
            );
        }
    }
    found.sort_by_key(|v| v.guarantee);
    found
}

/// Panic with the first violation, the seed and a one-line replay command.
///
/// # Panics
/// When any guarantee does not hold.
pub fn assert_guarantees(world: &World) {
    let violations = check_guarantees(world, true);
    if let Some(first) = violations.first() {
        let all = violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ");
        panic!(
            "{}\n  {all}\n  first: {first}\n--- transcript ---\n{}",
            replay_line(world.seed()),
            world.transcript_text()
        );
    }
}
