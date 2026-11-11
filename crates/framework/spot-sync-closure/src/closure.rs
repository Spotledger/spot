//! Steps 1 to 4 of ADR-064 section 3: walk the references and collect the extras.

use crate::error::ClosureError;
use crate::model::{Limits, RecordKey, SourceImage};
use crate::traits::{DestinationKnows, ImageSource, References};
use std::collections::{BTreeMap, BTreeSet};

/// An extra and the extras it references (edges used to order prerequisite bundles).
#[derive(Debug)]
pub(crate) struct ExtraNode {
    pub image: SourceImage,
    /// Targets of this extra that are themselves extras of this closure.
    pub deps: BTreeSet<RecordKey>,
    /// All declared targets, resolved to extras once the walk ends.
    refs: Vec<RecordKey>,
}

/// The result of the walk.
#[derive(Debug, Default)]
pub(crate) struct Closure {
    pub extras: BTreeMap<RecordKey, ExtraNode>,
    pub out_of_scope: Vec<RecordKey>,
    /// Extras that reach a root or satellite through references: they form a cycle with it, so
    /// they must be in the root bundle, not before it.
    pub with_roots: BTreeSet<RecordKey>,
    /// Dependents withheld because their closure needs a held record: `(dependent, held)`, one
    /// pair per held record that blocks a dependent, sorted by dependent and then by held record.
    /// The held record itself is never an extra, so the pair is a record of who waits for what.
    pub withheld: Vec<(RecordKey, RecordKey)>,
    /// Every dependent in `withheld`, plus every root or satellite the destination holds.
    pub withheld_keys: BTreeSet<RecordKey>,
}

/// Who references what, for the withholding below: target -> records that reference it.
type Dependents = BTreeMap<RecordKey, BTreeSet<RecordKey>>;

fn add_edges(dependents: &mut Dependents, dependent: &RecordKey, targets: &[RecordKey]) {
    for target in targets {
        dependents
            .entry(target.clone())
            .or_default()
            .insert(dependent.clone());
    }
}

/// A dependent cannot be applied without its dependency, so a dependent of a held (or of an
/// already withheld) record is withheld too.
///
/// The walk over `(dependent, held)` pairs is bounded: each pair enters `blocked` once, so a
/// reference cycle terminates.
fn withhold(
    dependents: &Dependents,
    held: &BTreeSet<RecordKey>,
    sendable: &BTreeSet<RecordKey>,
) -> BTreeMap<RecordKey, BTreeSet<RecordKey>> {
    let mut blocked: BTreeMap<RecordKey, BTreeSet<RecordKey>> = BTreeMap::new();
    let mut queue: Vec<(RecordKey, RecordKey)> = Vec::new();
    for key in held {
        for dependent in dependents.get(key).into_iter().flatten() {
            queue.push((dependent.clone(), key.clone()));
        }
    }
    while let Some((dependent, held_key)) = queue.pop() {
        if dependent == held_key || !sendable.contains(&dependent) {
            continue;
        }
        if blocked
            .entry(dependent.clone())
            .or_default()
            .insert(held_key.clone())
        {
            for further in dependents.get(&dependent).into_iter().flatten() {
                queue.push((further.clone(), held_key.clone()));
            }
        }
    }
    blocked
}

/// Collect the extras of `seeds` (roots and satellites).
///
/// A target is skipped when it is in the bundle already, the destination owns it, it is in the
/// destination's extras set, or it is in scope with its latest event at or before the acknowledged
/// position. A target the destination holds is never added as an extra, whatever its role, even
/// when the acknowledged position is below it (the hold is the reason it is absent there); the
/// records that need it are withheld instead. Anything else, including a target whose position
/// cannot be compared, joins as an extra and its own references are followed. Each record is added
/// once, so cycles terminate.
pub(crate) fn collect(
    seeds: &[SourceImage],
    dest: &dyn DestinationKnows,
    src: &dyn ImageSource,
    refs: &dyn References,
    limits: &Limits,
) -> Result<Closure, ClosureError> {
    let mut included: BTreeSet<RecordKey> = seeds.iter().map(|s| s.key.clone()).collect();
    let mut known: BTreeSet<RecordKey> = BTreeSet::new(); // targets already judged "destination has it"
    let mut out = Closure::default();
    let mut dependents = Dependents::new();
    let mut held: BTreeSet<RecordKey> = BTreeSet::new();
    let mut work: Vec<RecordKey> = Vec::new();
    for seed in seeds {
        let targets = refs.references(&seed.key.entity, &seed.image)?;
        add_edges(&mut dependents, &seed.key, &targets);
        work.extend(targets);
    }
    // A root or satellite the destination cannot decode is not sent either: its content must not
    // reach the node by any door.
    let held_seeds: BTreeSet<RecordKey> = seeds
        .iter()
        .filter(|s| dest.held(&s.key))
        .map(|s| s.key.clone())
        .collect();
    held.extend(held_seeds.iter().cloned());
    while let Some(target) = work.pop() {
        if included.contains(&target) || known.contains(&target) {
            continue;
        }
        if dest.owns(&target) || dest.extras().contains(&target) {
            known.insert(target);
            continue;
        }
        if dest.held(&target) {
            // Held: never an extra. The records that reference it are withheld below.
            held.insert(target);
            continue;
        }
        let image = src
            .image(&target)?
            .ok_or_else(|| ClosureError::TargetMissing {
                entity: target.entity.clone(),
                id: target.id.clone(),
            })?;
        let in_scope = dest.in_scope(&target);
        if in_scope
            && dest
                .acked_position()
                .is_some_and(|acked| image.position <= acked)
        {
            known.insert(target);
            continue;
        }
        if included.len() >= limits.hard_limit_records {
            return Err(ClosureError::TooLarge {
                count: included.len() + 1,
                limit: limits.hard_limit_records,
            });
        }
        let own = refs.references(&target.entity, &image.image)?;
        work.extend(own.iter().cloned());
        if !in_scope {
            out.out_of_scope.push(target.clone());
        }
        included.insert(target.clone());
        out.extras.insert(
            target,
            ExtraNode {
                image,
                deps: BTreeSet::new(),
                refs: own,
            },
        );
    }
    let members: BTreeSet<RecordKey> = out.extras.keys().cloned().collect();
    for (key, node) in out.extras.iter_mut() {
        node.deps = node
            .refs
            .iter()
            .filter(|r| members.contains(*r))
            .cloned()
            .collect();
        add_edges(&mut dependents, key, &node.refs);
    }
    let seed_keys: BTreeSet<&RecordKey> = seeds.iter().map(|s| &s.key).collect();
    let mut marked: BTreeSet<RecordKey> = out
        .extras
        .iter()
        .filter(|(_, n)| n.refs.iter().any(|r| seed_keys.contains(r)))
        .map(|(k, _)| k.clone())
        .collect();
    loop {
        let more: Vec<RecordKey> = out
            .extras
            .iter()
            .filter(|(k, n)| !marked.contains(*k) && n.deps.iter().any(|d| marked.contains(d)))
            .map(|(k, _)| k.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        marked.extend(more);
    }
    out.with_roots = marked;

    let blocked = withhold(&dependents, &held, &included);
    out.withheld = blocked
        .iter()
        .flat_map(|(dependent, blockers)| {
            blockers
                .iter()
                .map(move |blocker| (dependent.clone(), blocker.clone()))
        })
        .collect();
    out.withheld_keys = blocked.into_keys().collect();
    out.withheld_keys.extend(held_seeds);
    Ok(out)
}
