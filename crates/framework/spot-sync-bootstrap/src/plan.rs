//! The snapshot plan: which in-scope records exist, in which dependency-ordered groups.
//!
//! The plan is the manifest of ADR-066 section 3, step 2. It is made by one forward pass over the
//! snapshot, keeps **keys, sizes and link edges only** (never an image), and is deterministic: the
//! same snapshot, scope and node make the same plan, so a resumed session rebuilds it and checks
//! it against the cursor's digest instead of persisting it.

use crate::error::BootstrapError;
use crate::graph::components;
use crate::hash::Fnv;
use crate::source::{Seams, SnapshotSource};
use spot_sync_closure::{Limits, RecordKey};
use std::collections::BTreeMap;

/// One group: a record, or a cycle of records that must travel together.
#[derive(Debug, Clone)]
pub(crate) struct Group {
    pub(crate) keys: Vec<RecordKey>,
    /// Redacted image bytes of every record in the group (compact JSON length).
    pub(crate) bytes: usize,
}

/// The ordered groups plus what was left out.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) groups: Vec<Group>,
    pub(crate) records: usize,
    pub(crate) out_of_scope: usize,
    pub(crate) dangling_links: usize,
    pub(crate) digest: u64,
}

/// The size a redacted image takes in a bundle.
pub(crate) fn image_bytes(image: &serde_json::Value) -> usize {
    image.to_string().len()
}

pub(crate) fn build(
    source: &dyn SnapshotSource,
    seams: Seams<'_>,
    limits: &Limits,
    identity: u64,
) -> Result<Plan, BootstrapError> {
    let mut keys: Vec<RecordKey> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    let mut links: Vec<Vec<RecordKey>> = Vec::new();
    let mut out_of_scope = 0_usize;

    source.scan(&mut |record| {
        let key = RecordKey::new(&record.entity, &record.id);
        if keys.last().is_some_and(|last| *last >= key) {
            return Err(BootstrapError::SourceFailed {
                reason: "the snapshot images are not in strictly ascending order".to_owned(),
            });
        }
        // Scope first, before anything reads the image any further.
        if !seams.scope.covers(&key, &record.image) {
            out_of_scope += 1;
            return Ok(());
        }
        let targets = seams
            .references
            .references(&record.entity, &record.image)
            .map_err(|_| BootstrapError::ReferenceUnreadable {
                entity: record.entity.clone(),
            })?;
        let mut redacted = record.image;
        seams.redactor.redact_down(&record.entity, &mut redacted);
        sizes.push(image_bytes(&redacted));
        links.push(targets);
        keys.push(key);
        Ok(())
    })?;

    let index_of: BTreeMap<&RecordKey, usize> =
        keys.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let mut dangling_links = 0_usize;
    let mut edges: Vec<Vec<usize>> = Vec::with_capacity(keys.len());
    for (i, targets) in links.iter().enumerate() {
        let mut row: Vec<usize> = Vec::new();
        for target in targets {
            match index_of.get(target) {
                Some(&t) if t != i => row.push(t),
                Some(_) => {}
                // Not in this snapshot or not in the node's scope: not delivered (the scope is
                // strict), counted, and left to pull-on-miss repair (ADR-064).
                None => dangling_links += 1,
            }
        }
        row.sort_unstable();
        row.dedup();
        edges.push(row);
    }

    let mut hash = Fnv::new();
    hash.u64(identity);
    let mut groups = Vec::new();
    for component in components(&edges) {
        let bytes: usize = component.iter().map(|i| sizes[*i]).sum();
        if bytes > limits.max_bytes || component.len() > limits.hard_limit_records {
            return Err(BootstrapError::ComponentTooLarge {
                records: component.len(),
                bytes,
            });
        }
        hash.u64(component.len() as u64);
        let mut group_keys = Vec::with_capacity(component.len());
        for i in component {
            hash.str(&keys[i].entity);
            hash.str(&keys[i].id);
            group_keys.push(keys[i].clone());
        }
        groups.push(Group {
            keys: group_keys,
            bytes,
        });
    }
    Ok(Plan {
        groups,
        records: keys.len(),
        out_of_scope,
        dangling_links,
        digest: hash.finish(),
    })
}
