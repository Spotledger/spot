//! Resolving a merge: apply one choice per conflict, in conflict order, and rebuild the bundle.
//!
//! Every conflict must be resolved or the call fails with
//! [`bundle_merge.unresolved_conflict`](crate::error::codes::UNRESOLVED_CONFLICT); a resolution
//! naming an unknown conflict or naming one twice is refused too. Nothing is chosen silently.

use std::collections::BTreeMap;

use spot_custom_bundle::Files;

use crate::error::{BundleMergeError, codes};
use crate::items::{ItemMap, build_bundle};
use crate::model::{
    Choice, Edit, MergeResult, Resolution, ResolutionEntry, ResolutionLog, Resolved,
};

/// Applies one choice per conflict and returns the complete bundle and the log of who chose what.
///
/// Each [`Choice`] applies the conflict's matching [`Variants`](crate::Variants) edits; a
/// [`Choice::Custom`] replaces the conflict's primary item only, leaving the other items as they
/// merged. The bundle is rebuilt with our side's header, exactly as [`merge3`](crate::merge3) built
/// the clean merge.
pub fn resolve(
    result: &MergeResult,
    resolutions: &[Resolution],
) -> Result<Resolved, BundleMergeError> {
    let mut by_id: BTreeMap<&str, &Resolution> = BTreeMap::new();
    for resolution in resolutions {
        if by_id
            .insert(resolution.conflict.as_str(), resolution)
            .is_some()
        {
            return Err(BundleMergeError::about(
                codes::DUPLICATE_RESOLUTION,
                &resolution.conflict,
                "the same conflict is resolved twice",
            ));
        }
    }
    for id in by_id.keys() {
        if result.conflict(id).is_none() {
            return Err(BundleMergeError::about(
                codes::UNKNOWN_CONFLICT,
                *id,
                "the merge result carries no conflict with this id",
            ));
        }
    }

    let mut clean = result.clean.clone();
    let mut unknown = result.unknown.clone();
    let mut entries: Vec<ResolutionEntry> = Vec::new();
    for conflict in &result.conflicts {
        let resolution = by_id.get(conflict.id.as_str()).ok_or_else(|| {
            BundleMergeError::about(
                codes::UNRESOLVED_CONFLICT,
                &conflict.id,
                "every conflict must be resolved before the bundle can be built",
            )
        })?;
        let edits: Vec<Edit> = match &resolution.choice {
            Choice::Ours => conflict.variants.ours.clone(),
            Choice::Theirs => conflict.variants.theirs.clone(),
            Choice::Base => conflict.variants.base.clone(),
            Choice::Custom(bytes) => vec![Edit {
                section: conflict.primary.section.clone(),
                key: conflict.primary.key.clone(),
                value: Some(bytes.clone()),
            }],
        };
        apply_edits(&mut clean, &mut unknown, &edits);
        entries.push(ResolutionEntry {
            conflict: conflict.id.clone(),
            kind: conflict.kind,
            choice: resolution.choice.clone(),
            by: resolution.by.clone(),
            note: resolution.note.clone(),
        });
    }

    let bundle = build_bundle(&result.header, &clean, &unknown, &result.limits)?;
    Ok(Resolved {
        bundle,
        log: ResolutionLog { entries },
    })
}

/// Applies a batch of edits to the known and unknown section maps.
fn apply_edits(
    clean: &mut BTreeMap<String, ItemMap>,
    unknown: &mut BTreeMap<String, Files>,
    edits: &[Edit],
) {
    for edit in edits {
        if let Some(map) = clean.get_mut(&edit.section) {
            apply_edit(map, edit);
        } else if let Some(files) = unknown.get_mut(&edit.section) {
            apply_edit(files, edit);
        }
    }
}

/// Applies one edit: set the item's bytes, or remove it.
fn apply_edit(map: &mut BTreeMap<String, Vec<u8>>, edit: &Edit) {
    match &edit.value {
        Some(value) => {
            map.insert(edit.key.clone(), value.clone());
        }
        None => {
            map.remove(&edit.key);
        }
    }
}
