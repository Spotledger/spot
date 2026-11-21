//! The section-aware semantic checks: breaks a clean item-wise merge can still leave because they
//! span more than one item.
//!
//! - an override that survives a merge which removed the custom field it targets;
//! - a custom field that anchors (`insert_after`) after a field the merge removed;
//! - two sides that anchor different custom fields to the same predecessor (ambiguous order);
//! - two sides that changed different scripts bound to the same hook point;
//! - the merged metadata failing `spot_meta::Registry::build` against the app definitions.
//!
//! Two sides adding a custom field with the same **name** is not a separate semantic kind: an
//! item's key is `<doctype>.<name>`, so the two additions meet on the same key and are reported as
//! the item-level [`ConflictKind::BothAddedDifferently`](crate::ConflictKind::BothAddedDifferently)
//! (or merge cleanly when they are byte-identical).
//!
//! Each finding is a [`ConflictKind::Semantic`] conflict whose `ours`/`theirs`/`base` variants are
//! the two sides' and the ancestor's versions of every item it covers.

use std::collections::{BTreeMap, BTreeSet};

use spot_meta::{Customisations, Definition, Registry};

use crate::items::{ItemMap, MetaKey, field_anchor, meta_key, parse_meta, script_hook};
use crate::model::{Conflict, ConflictKind, Edit, ItemVersion, SemanticKind, Variants};

/// The three versions of one section's items.
type Sides<'a> = &'a (ItemMap, ItemMap, ItemMap);

/// Runs every semantic check over the cleanly merged items.
pub(crate) fn check(
    sides: &BTreeMap<String, (ItemMap, ItemMap, ItemMap)>,
    clean: &BTreeMap<String, ItemMap>,
    definitions: &[Definition],
    conflicts: &mut Vec<Conflict>,
) {
    if let Some(meta) = sides.get("meta") {
        if let Some(clean_meta) = clean.get("meta") {
            check_override_on_removed_field(meta, clean_meta, conflicts);
            check_insert_after(meta, clean_meta, conflicts);
        }
        check_invalid_meta(definitions, clean, meta, conflicts);
    }
    if let Some(scripts) = sides.get("scripts")
        && let Some(clean_scripts) = clean.get("scripts")
    {
        check_script_hooks(scripts, clean_scripts, conflicts);
    }
}

/// The three versions of one key, for a conflict.
fn version(section: &str, key: &str, sides: Sides<'_>) -> ItemVersion {
    ItemVersion {
        section: section.to_owned(),
        key: key.to_owned(),
        base: sides.0.get(key).cloned(),
        ours: sides.1.get(key).cloned(),
        theirs: sides.2.get(key).cloned(),
    }
}

/// The per-choice edits for a set of keys: each choice takes that side's version of every key.
fn side_variants(section: &str, keys: &[String], sides: Sides<'_>) -> Variants {
    let edits = |map: &ItemMap| -> Vec<Edit> {
        keys.iter()
            .map(|key| Edit {
                section: section.to_owned(),
                key: key.clone(),
                value: map.get(key).cloned(),
            })
            .collect()
    };
    Variants {
        ours: edits(&sides.1),
        theirs: edits(&sides.2),
        base: edits(&sides.0),
    }
}

/// A semantic conflict over `involved` keys, the first key being the primary.
fn semantic_conflict(
    section: &str,
    kind: SemanticKind,
    involved: &[String],
    explanation: String,
    sides: Sides<'_>,
) -> Conflict {
    let primary = version(section, &involved[0], sides);
    let related = involved[1..]
        .iter()
        .map(|key| version(section, key, sides))
        .collect();
    Conflict {
        id: String::new(),
        kind: ConflictKind::Semantic(kind),
        primary,
        related,
        explanation,
        variants: side_variants(section, involved, sides),
        upgrade: None,
    }
}

/// An override that survives a merge which removed the custom field it targets.
fn check_override_on_removed_field(
    sides: Sides<'_>,
    clean: &ItemMap,
    conflicts: &mut Vec<Conflict>,
) {
    let mut by_field: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in clean.keys() {
        if let Some(MetaKey::Override {
            doctype,
            field: Some(field),
            ..
        }) = meta_key(key)
        {
            let field_key = format!("{doctype}.{field}");
            if clean.get(&field_key).is_none() {
                by_field.entry(field_key).or_default().push(key.clone());
            }
        }
    }
    for (field_key, mut overrides) in by_field {
        overrides.sort();
        let removed =
            |map: &ItemMap| sides.0.contains_key(&field_key) && !map.contains_key(&field_key);
        if !removed(&sides.1) && !removed(&sides.2) {
            continue;
        }
        let mut involved = vec![field_key.clone()];
        involved.extend(overrides.iter().cloned());
        let explanation = format!(
            "one side removed the custom field `{field_key}` while the other kept an override on it \
             ({}); the merged metadata names a field that no longer exists",
            overrides.join(", ")
        );
        conflicts.push(semantic_conflict(
            "meta",
            SemanticKind::OverrideOnRemovedField,
            &involved,
            explanation,
            sides,
        ));
    }
}

/// A custom field anchored after a removed field, and two sides anchoring to the same predecessor.
fn check_insert_after(sides: Sides<'_>, clean: &ItemMap, conflicts: &mut Vec<Conflict>) {
    let mut anchored: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, bytes) in clean {
        if let Some(MetaKey::Field { doctype, .. }) = meta_key(key)
            && let Some(Some(anchor)) = field_anchor(bytes)
        {
            anchored
                .entry(format!("{doctype}.{anchor}"))
                .or_default()
                .push(key.clone());
        }
    }

    // A field anchored after a field that was a customisation and the merge removed.
    for (anchor_key, fields) in &anchored {
        if clean.get(anchor_key).is_some() {
            continue;
        }
        let existed = sides.0.contains_key(anchor_key)
            || sides.1.contains_key(anchor_key)
            || sides.2.contains_key(anchor_key);
        if !existed {
            continue;
        }
        let mut involved = vec![fields[0].clone(), anchor_key.clone()];
        involved.extend(fields.iter().skip(1).cloned());
        let explanation = format!(
            "`{}` anchors after `{anchor_key}`, which the merge removed",
            fields[0]
        );
        conflicts.push(semantic_conflict(
            "meta",
            SemanticKind::InsertAfterRemovedField,
            &involved,
            explanation,
            sides,
        ));
    }

    // Two sides anchoring different fields to the same predecessor: the order is ambiguous.
    for (anchor_key, fields) in &anchored {
        if fields.len() < 2 {
            continue;
        }
        let changed = |map: &ItemMap| -> BTreeSet<String> {
            fields
                .iter()
                .filter(|key| map.get(*key) != sides.0.get(*key))
                .cloned()
                .collect()
        };
        let ours = changed(&sides.1);
        let theirs = changed(&sides.2);
        if ours.is_empty() || theirs.is_empty() || ours == theirs {
            continue;
        }
        let mut involved = fields.clone();
        involved.sort();
        let explanation = format!(
            "both sides anchored different custom fields to `{anchor_key}` ({}), so their order is \
             ambiguous",
            fields.join(", ")
        );
        conflicts.push(semantic_conflict(
            "meta",
            SemanticKind::InsertAfterCollision,
            &involved,
            explanation,
            sides,
        ));
    }
}

/// Two sides that changed different scripts bound to the same hook point.
fn check_script_hooks(sides: Sides<'_>, clean: &ItemMap, conflicts: &mut Vec<Conflict>) {
    let mut by_hook: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, bytes) in clean {
        if let Some(hook) = script_hook(bytes) {
            by_hook.entry(hook).or_default().push(key.clone());
        }
    }
    for (hook, keys) in by_hook {
        if keys.len() < 2 {
            continue;
        }
        let changed = |map: &ItemMap| -> BTreeSet<String> {
            keys.iter()
                .filter(|key| map.get(*key) != sides.0.get(*key))
                .cloned()
                .collect()
        };
        let ours = changed(&sides.1);
        let theirs = changed(&sides.2);
        if ours.is_empty() || theirs.is_empty() || ours == theirs {
            continue;
        }
        let mut involved = keys.clone();
        involved.sort();
        let explanation = format!(
            "both sides changed different scripts bound to hook point `{hook}` ({}), so the hook \
             runs two changed scripts",
            involved.join(", ")
        );
        conflicts.push(semantic_conflict(
            "scripts",
            SemanticKind::ScriptHookCollision,
            &involved,
            explanation,
            sides,
        ));
    }
}

/// The merged metadata rebuilt over the app definitions, reported as `invalid_meta` conflicts.
fn check_invalid_meta(
    definitions: &[Definition],
    clean: &BTreeMap<String, ItemMap>,
    sides: Sides<'_>,
    conflicts: &mut Vec<Conflict>,
) {
    if definitions.is_empty() {
        return;
    }
    let Some(clean_meta) = clean.get("meta") else {
        return;
    };
    let mut customisations = Customisations::new();
    if parse_meta(clean_meta, &mut customisations).is_err() {
        return;
    }
    let Err(errors) = Registry::build(definitions.to_vec(), &customisations) else {
        return;
    };
    for error in errors {
        let item = match (&error.doctype, &error.field) {
            (Some(doctype), Some(field)) => format!("{doctype}.{field}"),
            (Some(doctype), None) => doctype.clone(),
            (None, Some(field)) => field.clone(),
            (None, None) => "meta".to_owned(),
        };
        let explanation =
            format!("the merged metadata does not build against the app definitions: {error}");
        conflicts.push(semantic_conflict(
            "meta",
            SemanticKind::InvalidMeta,
            std::slice::from_ref(&item),
            explanation,
            sides,
        ));
    }
}
