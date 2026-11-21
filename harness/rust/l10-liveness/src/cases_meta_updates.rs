//! The scenarios that compare whole change sequences (S09–S11): an app update's removal and
//! collision halves, the commuting and non-commuting pairs, and the replay determinism. Each
//! is a comment-cited ADR rule; the table in `scenarios.rs` records them.

use crate::LivenessHost;
use crate::cases_single_change::field_shape;
use crate::change::{Change, FieldSpecInput, SetProperty};
use crate::checks::{apply_ok, check, check_eq, views_debug};
use crate::host::StandardHost;

use super::scenarios::standard_defs;

// -------------------------------------------------------------------------------------------
// S08 — the first app-update scenario
// -------------------------------------------------------------------------------------------

/// S08 — ADR-05/ADR-068 §1.1: an app update replaces the standard definitions; the host merges
/// the site's custom fields and overrides **over** them, so the update must not clobber
/// either.
pub(super) fn s08_update_keeps_customisations(
    host: &mut dyn crate::LivenessHost,
) -> Result<Vec<String>, String> {
    apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_channel").with_label("Channel"),
            insert_after: None,
        },
    )?;
    apply_ok(
        host,
        &Change::SetProperty {
            doctype: "party".to_owned(),
            field: Some("code".to_owned()),
            property: SetProperty::Required(true),
        },
    )?;

    // The app update: a new standard field, everything else unchanged.
    let mut defs = standard_defs();
    defs[0]
        .fields
        .push(FieldSpecInput::text("phone").with_label("Phone"));
    apply_ok(host, &Change::ReplaceDefinitions { definitions: defs })?;

    let meta = host.meta();
    let names: Vec<String> = meta
        .definition("party")
        .ok_or("party vanished")?
        .fields
        .iter()
        .map(|f| f.name.clone())
        .collect();
    check(
        names.iter().any(|n| n == "custom_channel"),
        "the site's custom field survived the update",
    )?;
    check(
        names.iter().any(|n| n == "phone"),
        "the new standard field arrived",
    )?;
    check_eq(
        field_shape(&meta, "party", "code").map(|f| f.required),
        Some(true),
        "the site's override survived the update",
    )?;
    Ok(Vec::new())
}

// -------------------------------------------------------------------------------------------
// S09..S11
// -------------------------------------------------------------------------------------------

/// S09 — ADR-05: an app update that removes a standard field no customisation references
/// succeeds; an update that would make a site's custom field name collide with a standard
/// field is refused **with the colliding field reported**, and the site's field stays.
pub(super) fn s09_update_removal_and_collision(
    host: &mut dyn crate::LivenessHost,
) -> Result<Vec<String>, String> {
    // (a) removal of an unreferenced standard field succeeds.
    let mut defs = standard_defs();
    defs[0].fields.retain(|f| f.name != "state");
    apply_ok(
        host,
        &Change::ReplaceDefinitions {
            definitions: defs.clone(),
        },
    )?;
    check(
        host.meta()
            .definition("party")
            .is_some_and(|d| d.fields.iter().all(|f| f.name != "state")),
        "the removed standard field is gone",
    )?;

    // (b) a site's custom field whose name a new standard field would take must be protected:
    // the update is refused and names the colliding field.
    apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_state_extra"),
            insert_after: None,
        },
    )?;
    let mut colliding = defs;
    colliding[0]
        .fields
        .push(FieldSpecInput::text("custom_state_extra"));
    match host.apply(&Change::ReplaceDefinitions {
        definitions: colliding,
    }) {
        Err(e) => check_eq(
            e.refusal.collision_field(),
            Some("custom_state_extra"),
            "the refusal names the colliding field",
        ),
        Ok(_) => {
            Err("an update that makes a custom field's name collide must be refused".to_owned())
        }
    }?;
    check(
        host.meta()
            .definition("party")
            .is_some_and(|d| d.fields.iter().any(|f| f.name == "custom_state_extra")),
        "the site's custom field survives the refused update",
    )?;
    Ok(Vec::new())
}

/// S10 — ADR-05: changes that commute give the same final effective metadata in either order;
/// changes that do not commute give different, deterministic results (the later override of the
/// same property wins, spot-meta's documented rule — any host's order rule must be fixed, not
/// incidental, which is what the determinism halves pin).
pub(super) fn s10_commuting_and_order_dependent_pairs() -> Result<Vec<String>, String> {
    // Commuting pair 1: custom fields on different DocTypes.
    let a = Change::AddCustomField {
        doctype: "party".to_owned(),
        spec: FieldSpecInput::text("custom_a"),
        insert_after: None,
    };
    let b = Change::AddCustomField {
        doctype: "invoice".to_owned(),
        spec: FieldSpecInput::text("custom_b"),
        insert_after: None,
    };
    check_eq(
        replay_pair(&a, &b)?,
        replay_pair(&b, &a)?,
        "custom fields on different DocTypes commute",
    )?;

    // Commuting pair 2: an override and an unrelated DocType's custom field.
    let o = Change::SetProperty {
        doctype: "invoice".to_owned(),
        field: Some("state".to_owned()),
        property: SetProperty::Label("State".to_owned()),
    };
    check_eq(
        replay_pair(&a, &o)?,
        replay_pair(&o, &a)?,
        "an override and an unrelated custom field commute",
    )?;

    // Non-commuting pair: two overrides of the same property. In each order the result is the
    // later override winning (deterministic), and the two orders differ.
    let p1 = Change::SetProperty {
        doctype: "party".to_owned(),
        field: Some("code".to_owned()),
        property: SetProperty::Label("First".to_owned()),
    };
    let p2 = Change::SetProperty {
        doctype: "party".to_owned(),
        field: Some("code".to_owned()),
        property: SetProperty::Label("Second".to_owned()),
    };
    let ab = replay_pair(&p1, &p2)?;
    let ba = replay_pair(&p2, &p1)?;
    check(ab != ba, "the non-commuting pair gives different results")?;
    check_eq(
        replay_pair(&p1, &p2)?,
        ab.clone(),
        "the order (p1, p2) is deterministic",
    )?;
    check_eq(
        replay_pair(&p2, &p1)?,
        ba.clone(),
        "the order (p2, p1) is deterministic",
    )?;
    check_eq(
        label_of(&ab, "party", "code", "Second"),
        true,
        "in (p1, p2) the later override wins",
    )?;
    check_eq(
        label_of(&ba, "party", "code", "First"),
        true,
        "in (p2, p1) the later override wins",
    )?;
    Ok(vec![
        "commuting pairs agree; same-property overrides settle deterministically".to_owned(),
    ])
}

/// S11 — ADR-05: the same sequence of changes replayed from empty metadata reproduces the same
/// metadata version and the same views byte for byte (deterministic compilation).
pub(super) fn s11_replay_determinism() -> Result<Vec<String>, String> {
    let first = replay_sequence()?;
    let second = replay_sequence()?;
    check_eq(second.views, first.views, "the views byte for byte")?;
    check_eq(second.version, first.version, "the final metadata version")?;
    check_eq(
        second.last_reported_version,
        first.last_reported_version,
        "the version the last step reported",
    )?;
    Ok(vec![
        "replay reproduced the same version and the same views".to_owned(),
    ])
}

// -------------------------------------------------------------------------------------------
// Shared replay helpers
// -------------------------------------------------------------------------------------------

/// Whether the replay's views contain a view whose rendered form carries `doctype` and
/// `field` with `label` (the later override's visible outcome).
fn label_of(snapshot: &ReplaySnapshot, doctype: &str, field: &str, label: &str) -> bool {
    snapshot
        .views
        .iter()
        .any(|v| v.contains(&format!("name: {doctype}")) && v.contains(field) && v.contains(label))
        || snapshot
            .views
            .iter()
            .any(|v| v.contains(doctype) && v.contains(field) && v.contains(label))
}

/// The final state of one replay: the final version, the version the last step reported, and
/// every view's rendered form.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplaySnapshot {
    version: u64,
    last_reported_version: u64,
    views: Vec<String>,
}

/// Applies `a` then `b` on a fresh standard host; the final version and views.
fn replay_pair(a: &Change, b: &Change) -> Result<ReplaySnapshot, String> {
    let mut host = StandardHost::new();
    let _a_version = host
        .apply(a)
        .map_err(|e| format!("step refused: {e}"))?
        .new_version;
    let last_reported_version = host
        .apply(b)
        .map_err(|e| format!("step refused: {e}"))?
        .new_version;
    let meta = host.meta();
    Ok(ReplaySnapshot {
        version: meta.version(),
        last_reported_version,
        views: views_debug(&meta),
    })
}

/// Applies the standard sequence on a fresh standard host; the final version and views.
fn replay_sequence() -> Result<ReplaySnapshot, String> {
    let mut host = StandardHost::new();
    let mut last_reported_version = 0;
    for change in standard_sequence() {
        last_reported_version = host
            .apply(&change)
            .map_err(|e| format!("step refused: {e}"))?
            .new_version;
    }
    let meta = host.meta();
    Ok(ReplaySnapshot {
        version: meta.version(),
        last_reported_version,
        views: views_debug(&meta),
    })
}

/// The standard sequence the determinism scenario replays: adds, an insert-after add, an
/// override, a posted-flag change and an app update.
fn standard_sequence() -> Vec<Change> {
    vec![
        Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_a").with_label("A"),
            insert_after: None,
        },
        Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_b").with_default("x"),
            insert_after: Some("custom_a".to_owned()),
        },
        Change::AddCustomField {
            doctype: "invoice".to_owned(),
            spec: FieldSpecInput::select("custom_state_extra", &["Held", "Sent"]),
            insert_after: None,
        },
        Change::SetProperty {
            doctype: "party".to_owned(),
            field: Some("code".to_owned()),
            property: SetProperty::Label("Code".to_owned()),
        },
        Change::SetProperty {
            doctype: "invoice".to_owned(),
            field: Some("state".to_owned()),
            property: SetProperty::AllowOnPosted(true),
        },
        Change::ReplaceDefinitions {
            definitions: standard_defs(),
        },
    ]
}
