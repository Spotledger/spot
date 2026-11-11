//! `build`: closure, redaction, splitting.

use crate::closure::{Closure, collect};
use crate::error::ClosureError;
use crate::model::{
    Bundle, BundleKind, BundlePlan, BundleRecord, Limits, RecordKey, Role, SourceImage,
};
use crate::split::{components, pack};
use crate::traits::{DestinationKnows, ImageSource, Redactor, References};
use std::collections::BTreeMap;

fn record(role: Role, src: &SourceImage, redactor: &dyn Redactor) -> (BundleRecord, usize) {
    let mut image = src.image.clone();
    redactor.redact_down(&src.key.entity, &mut image);
    let bytes = image.to_string().len();
    (
        BundleRecord {
            role,
            key: src.key.clone(),
            position: src.position,
            image,
        },
        bytes,
    )
}

fn bundle(kind: BundleKind, parts: Vec<(BundleRecord, usize)>) -> Bundle {
    let bytes = parts.iter().map(|(_, b)| b).sum();
    Bundle {
        kind,
        records: parts.into_iter().map(|(r, _)| r).collect(),
        bytes,
    }
}

/// Add a bundle, unless the records it would carry were all withheld: an empty bundle would wake
/// the node for nothing.
fn push(bundles: &mut Vec<Bundle>, kind: BundleKind, parts: Vec<(BundleRecord, usize)>) {
    if !parts.is_empty() {
        bundles.push(bundle(kind, parts));
    }
}

/// Build the bundles for one source transaction going to one destination.
///
/// `roots` are the documents the user saved or submitted; `satellites` are the other in-scope
/// events of the same source transaction. Every image gets `excluded_fields.down` applied through
/// `redactor` (references are read from the unredacted image, so a redaction can never hide a
/// dependency). The extras are the targets the destination lacks (see the crate docs), followed
/// recursively. When the whole closure fits one bundle (`limits.max_records` and
/// `limits.max_bytes`) the plan is a single root bundle with the extras first. Otherwise the extras
/// become prerequisite bundles in dependency order (a cycle stays in one bundle) followed by the
/// root bundle holding the roots and satellites; a source transaction is never split. Each bundle is
/// complete in itself given the ones before it.
///
/// A target the destination holds is never an extra (ADR-066 section 5). A root, satellite or
/// extra whose closure needs such a record cannot be applied by the destination, so it is
/// withheld with it: it is not in `bundles` and it is reported in
/// [`BundlePlan::withheld_by_hold`].
///
/// # Errors
/// `InvalidLimits` for unusable limits, `TooLarge` over `limits.hard_limit_records`,
/// `TargetMissing` for a reference neither side has, plus whatever the traits return.
pub fn build(
    roots: &[SourceImage],
    satellites: &[SourceImage],
    dest: &dyn DestinationKnows,
    src: &dyn ImageSource,
    refs: &dyn References,
    redactor: &dyn Redactor,
    limits: Limits,
) -> Result<BundlePlan, ClosureError> {
    limits.validate()?;
    let seeds: Vec<SourceImage> = roots.iter().chain(satellites).cloned().collect();
    if seeds.len() > limits.hard_limit_records {
        return Err(ClosureError::TooLarge {
            count: seeds.len(),
            limit: limits.hard_limit_records,
        });
    }
    let Closure {
        mut extras,
        mut out_of_scope,
        with_roots,
        withheld,
        withheld_keys,
    } = collect(&seeds, dest, src, refs, &limits)?;
    // Nothing of a withheld record is sent, neither its image nor the extras only it needed.
    extras.retain(|key, _| !withheld_keys.contains(key));
    out_of_scope.retain(|key| !withheld_keys.contains(key));

    let mut own: Vec<(BundleRecord, usize)> = Vec::new();
    own.extend(
        satellites
            .iter()
            .filter(|s| !withheld_keys.contains(&s.key))
            .map(|s| record(Role::Satellite, s, redactor)),
    );
    own.extend(
        roots
            .iter()
            .filter(|s| !withheld_keys.contains(&s.key))
            .map(|s| record(Role::Root, s, redactor)),
    );
    let mut extra_parts: BTreeMap<RecordKey, (BundleRecord, usize)> = extras
        .iter()
        .map(|(k, n)| (k.clone(), record(Role::Extra, &n.image, redactor)))
        .collect();

    let total_records = own.len() + extra_parts.len();
    let total_bytes: usize = own.iter().map(|(_, b)| b).sum::<usize>()
        + extra_parts.values().map(|(_, b)| b).sum::<usize>();
    // Extras in a cycle with a root cannot go earlier than the root: they join the root bundle.
    let (with_roots, ordered): (Vec<_>, Vec<_>) = components(&extras)
        .into_iter()
        .partition(|c| c.iter().any(|k| with_roots.contains(k)));
    let mut bundles = Vec::new();
    if total_records <= limits.max_records && total_bytes <= limits.max_bytes {
        let mut parts: Vec<(BundleRecord, usize)> = ordered
            .into_iter()
            .chain(with_roots)
            .flatten()
            .filter_map(|k| extra_parts.remove(&k))
            .collect();
        parts.extend(own);
        push(&mut bundles, BundleKind::Root, parts);
    } else {
        let size_of = |k: &RecordKey| extra_parts.get(k).map_or(0, |(_, b)| *b);
        let groups = pack(ordered, &size_of, limits.max_records, limits.max_bytes);
        for group in groups {
            let parts = group.iter().filter_map(|k| extra_parts.remove(k)).collect();
            push(&mut bundles, BundleKind::Prerequisite, parts);
        }
        let mut parts: Vec<(BundleRecord, usize)> = with_roots
            .into_iter()
            .flatten()
            .filter_map(|k| extra_parts.remove(&k))
            .collect();
        parts.extend(own);
        push(&mut bundles, BundleKind::Root, parts);
    }
    Ok(BundlePlan {
        bundles,
        out_of_scope_extras: out_of_scope,
        withheld_by_hold: withheld,
    })
}
