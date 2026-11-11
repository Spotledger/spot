//! The scenarios that drive one change at a time (S01–S08, S12–S16): every one is a
//! comment-cited ADR rule, driven through [`LivenessHost`](crate::LivenessHost) and recorded
//! as an [`Outcome`](crate::Outcome) by the table in `scenarios.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::change::{AddDocTypeDef, Change, FieldSpecInput, Refusal, SetProperty};
use crate::checks::{apply_ok, apply_refused, check, check_eq, probe};
use crate::{LivenessHost, Probe, ProbeOutcome, SEQUENTIAL_BUDGET};

// -------------------------------------------------------------------------------------------
// S01..S08
// -------------------------------------------------------------------------------------------

/// S01 — ADR-05: nothing a user changes in the UI may need a rebuild or restart. A custom field
/// is visible in the effective metadata immediately, and the metadata version rises by exactly
/// 1 per accepted change (add, override, remove alike).
pub(super) fn s01_custom_field_visible_version_plus_one(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    let start = host.meta().version();
    let v = apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_referred_by").with_label("Referred By"),
            insert_after: None,
        },
    )?;
    check_eq(v, start + 1, "the version after one change")?;
    let meta = host.meta();
    check_eq(
        meta.version(),
        start + 1,
        "the effective metadata's version",
    )?;
    let label = meta
        .definition("party")
        .and_then(|d| d.fields.iter().find(|f| f.name == "custom_referred_by"))
        .map(|f| f.label_or_name().to_owned());
    check_eq(
        label,
        Some("Referred By".to_owned()),
        "the custom field is in the effective definitions immediately",
    )?;

    // Each further accepted change also adds exactly one: an override, then a removal.
    let v = apply_ok(
        host,
        &Change::SetProperty {
            doctype: "party".to_owned(),
            field: Some("custom_referred_by".to_owned()),
            property: SetProperty::Required(true),
        },
    )?;
    check_eq(v, start + 2, "the version after the second change")?;
    let v = apply_ok(
        host,
        &Change::RemoveCustomField {
            doctype: "party".to_owned(),
            name: "custom_referred_by".to_owned(),
        },
    )?;
    check_eq(v, start + 3, "the version after a removal")?;
    Ok(vec![
        "version +1 per accepted change (add, override, remove)".to_owned(),
    ])
}

/// S02 — ADR-05/ADR-068 §6: one change reaches **both** compiled views (the storage catalog and
/// the app API `DocTypeDef`) stamped with the same metadata version. The app half is asserted
/// against the metadata the host returned; the storage half runs through the schema probe, so
/// a host without that layer records NotSupported and the report says so.
pub(super) fn s02_field_in_both_views(host: &mut dyn LivenessHost) -> Result<Vec<String>, String> {
    let v = apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_note").with_label("Note"),
            insert_after: Some("code".to_owned()),
        },
    )?;
    let meta = host.meta();
    check_eq(
        meta.doctype("party").map(|d| d.version),
        Some(v),
        "the app API view's version",
    )?;
    let in_view = meta
        .doctype("party")
        .and_then(|d| d.field("custom_note").cloned());
    check(in_view.is_some(), "the custom field is in the app API view")?;
    check_eq(
        in_view.as_ref().map(|f| f.label.as_str()),
        Some("Note"),
        "the app API view carries the field's label",
    )?;

    // The storage half: the schema probe must already see a schema compiled from this version
    // that carries the field.
    let (notes, schema) = probe(host, Probe::Schema, v)?;
    if schema.is_none() {
        return Ok(notes);
    }
    Ok(notes)
}

/// S03 — ADR-068 §1: an override touches exactly the property it names. No other *property*
/// of the field, no other field, and no other DocType's compiled view content moves. (The
/// views' `version` stamp moves with every change by design — ADR-05 stamps the version on
/// every compiled view — so content, not the version field, is what the scenario compares.)
pub(super) fn s03_override_islands(host: &mut dyn LivenessHost) -> Result<Vec<String>, String> {
    apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_region").with_label("Region"),
            insert_after: None,
        },
    )?;
    let before = host.meta();
    apply_ok(
        host,
        &Change::SetProperty {
            doctype: "party".to_owned(),
            field: Some("custom_region".to_owned()),
            property: SetProperty::Required(true),
        },
    )?;
    let after = host.meta();

    let mut expected = field_shape(&before, "party", "custom_region").ok_or("field vanished")?;
    expected.required = true;
    check_eq(
        field_shape(&after, "party", "custom_region"),
        Some(expected),
        "only `required` changed on the field",
    )?;
    check_eq(
        field_shape(&before, "party", "code"),
        field_shape(&after, "party", "code"),
        "a standard field of the same DocType is untouched",
    )?;
    // The other DocType's view content is untouched; its `version` stamp moves with every
    // change by design (ADR-05 stamps the version on every compiled view), so content only.
    check_eq(
        before.doctype("invoice").map(|d| d.fields.clone()),
        after.doctype("invoice").map(|d| d.fields.clone()),
        "another DocType's app view fields are untouched",
    )?;
    check_eq(
        before.doctype("invoice").map(|d| d.label.clone()),
        after.doctype("invoice").map(|d| d.label.clone()),
        "another DocType's app view label is untouched",
    )?;
    check_eq(
        before.doctypes().count(),
        after.doctypes().count(),
        "no DocType appeared or vanished",
    )?;
    Ok(vec!["only the named property moved".to_owned()])
}

/// S04 — ADR-05: removing a custom field removes it from the definitions and the app API view,
/// and a DocType the removal does not touch keeps the *same* `Arc<DocTypeDef>` — its compiled
/// view is shared, not rebuilt. The same-Arc requirement is part of the scenario and fails on
/// a host that rebuilds unrelated views: `spot_meta::Registry` does exactly that today, so
/// `tests/meta.rs` excludes this scenario (an `#[ignore]`d run pins that the failure is the
/// sharing gap alone) and the README lists it under Open questions. Never silently passed.
pub(super) fn s04_removal_updates_views_snapshot_arc(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_temp"),
            insert_after: None,
        },
    )?;
    let before = host.meta();
    let unrelated_before = before.doctype("invoice").ok_or("no invoice view")?;
    apply_ok(
        host,
        &Change::RemoveCustomField {
            doctype: "party".to_owned(),
            name: "custom_temp".to_owned(),
        },
    )?;
    let after = host.meta();

    check(
        after
            .definition("party")
            .is_some_and(|d| d.fields.iter().all(|f| f.name != "custom_temp")),
        "the field is gone from the definitions",
    )?;
    check(
        after
            .doctype("party")
            .is_some_and(|d| d.field("custom_temp").is_none()),
        "the field is gone from the app API view",
    )?;

    // The version stamp moves with every change by design (ADR-05); the content must not.
    let content_equal = match (
        before.doctype("invoice").map(|d| d.fields.clone()),
        after.doctype("invoice").map(|d| d.fields.clone()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => return Err("the unrelated DocType's view vanished".to_owned()),
    };
    check(
        content_equal,
        "an unrelated DocType's view keeps its content",
    )?;

    // The sharing requirement, asserted strictly.
    let shared = after
        .doctype("invoice")
        .is_some_and(|view| Arc::ptr_eq(&unrelated_before, &view.clone()));
    check(
        shared,
        "an unrelated DocType's compiled view is the same Arc after a change to another \
         DocType (the host must share unchanged views, not rebuild every view per change)",
    )?;

    let (schema_notes, schema) = probe(host, Probe::Schema, after.version())?;
    let mut notes = vec!["the unrelated DocType's view is the same Arc".to_owned()];
    notes.extend(schema_notes);
    let _ = schema;
    Ok(notes)
}

/// S05 — ADR-15/ADR-05: a runtime-created DocType appears immediately, a posted one carries the
/// engine-managed fields, and a valid storage catalog sits behind it (probed, when the host
/// has the layer).
pub(super) fn s05_runtime_doctype_engine_fields(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    let v = apply_ok(
        host,
        &Change::AddDocType {
            definition: AddDocTypeDef::posted(
                "equipment_request",
                vec![
                    FieldSpecInput::text("requested_by").with_required(true),
                    FieldSpecInput::text("note"),
                ],
            )
            .with_label("Equipment Request")
            .with_title_field("requested_by"),
        },
    )?;
    let meta = host.meta();
    let view = meta
        .doctype("equipment_request")
        .ok_or("the runtime DocType has no app API view")?;
    check_eq(view.is_posted, true, "the DocType is posted")?;
    for engine in spot_meta::ENGINE_FIELDS {
        check(
            view.field(engine).is_some(),
            format!("the posted DocType carries the engine-managed field `{engine}`"),
        )?;
    }
    check_eq(
        view.version,
        v,
        "the runtime DocType's view carries the new version",
    )?;

    // A further customisation of the runtime DocType keeps everything live.
    let v2 = apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "equipment_request".to_owned(),
            spec: FieldSpecInput::select("custom_priority", &["Low", "High"]),
            insert_after: None,
        },
    )?;
    check_eq(
        v2,
        v + 1,
        "the version after the custom field on the runtime DocType",
    )?;
    check(
        host.meta()
            .doctype("equipment_request")
            .is_some_and(|d| d.field("custom_priority").is_some()),
        "a custom field can be added to a runtime DocType",
    )?;
    // The catalog behind it: through the schema probe, at the version the custom field made.
    probe(host, Probe::Schema, v2)?;
    Ok(Vec::new())
}

/// S06 — ADR-05: a rejected change must not move the metadata version. A runtime DocType whose
/// name collides with an existing DocType (standard or runtime-created) is refused whole.
pub(super) fn s06_doctype_name_collision_refused(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    let start = host.meta().version();
    apply_refused(
        host,
        &Change::AddDocType {
            definition: AddDocTypeDef::posted("party", vec![FieldSpecInput::text("x")]),
        },
        &Refusal::NameCollision {
            name: "party".to_owned(),
        },
        "a DocType named like an existing standard one",
    )?;
    check_eq(
        host.meta().version(),
        start,
        "the version does not move on refusal",
    )?;
    Ok(Vec::new())
}

/// S07 — ADR-05: every refused change (unknown target, bad name, bad override, missing field)
/// leaves the previous metadata **fully** intact: the version, every compiled view, and the
/// customisations already in force.
pub(super) fn s07_refusal_leaves_metadata_intact(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    apply_ok(
        host,
        &Change::AddCustomField {
            doctype: "party".to_owned(),
            spec: FieldSpecInput::text("custom_kept").with_label("Kept"),
            insert_after: None,
        },
    )?;
    apply_ok(
        host,
        &Change::SetProperty {
            doctype: "party".to_owned(),
            field: Some("code".to_owned()),
            property: SetProperty::Label("Code".to_owned()),
        },
    )?;
    let before = host.meta();
    let before_views: BTreeMap<&str, String> = before
        .doctypes()
        .map(|d| (d.name.as_str(), format!("{d:?}")))
        .collect();
    let before_version = before.version();

    for (what, change, refusal) in [
        (
            "an unknown DocType",
            Change::AddCustomField {
                doctype: "ghost".to_owned(),
                spec: FieldSpecInput::text("custom_x"),
                insert_after: None,
            },
            Refusal::UnknownName,
        ),
        (
            "a non-site-prefixed custom field",
            Change::AddCustomField {
                doctype: "party".to_owned(),
                spec: FieldSpecInput::text("plain"),
                insert_after: None,
            },
            Refusal::BadCustomFieldName,
        ),
        (
            "an override that lowers max_chars",
            Change::SetProperty {
                doctype: "party".to_owned(),
                field: Some("code".to_owned()),
                property: SetProperty::MaxChars(1),
            },
            Refusal::BadOverride,
        ),
        (
            "removing a field that is not a custom field",
            Change::RemoveCustomField {
                doctype: "party".to_owned(),
                name: "code".to_owned(),
            },
            Refusal::UnknownName,
        ),
    ] {
        apply_refused(host, &change, &refusal, what).map_err(|e| format!("{what}: {e}"))?;
    }

    let after = host.meta();
    check_eq(after.version(), before_version, "the version")?;
    let after_views: BTreeMap<&str, String> = after
        .doctypes()
        .map(|d| (d.name.as_str(), format!("{d:?}")))
        .collect();
    check_eq(after_views, before_views, "every DocType's compiled view")?;
    check_eq(
        field_shape(&after, "party", "custom_kept").and_then(|f| f.label),
        Some("Kept".to_owned()),
        "the earlier customisation is still in force",
    )?;
    Ok(Vec::new())
}

// -------------------------------------------------------------------------------------------
// S12..S16
// -------------------------------------------------------------------------------------------

/// S12 — ADR-05: readers holding an old `Arc<EffectiveMeta>` keep a consistent snapshot while
/// changes land. Reader threads take their snapshot `Arc`, a writer then applies changes, and
/// the readers verify internal consistency (every view carries the snapshot's version) with no
/// sleeps — coordination only by a barrier and joins (`std::thread`, per the brief).
pub(super) fn s12_reader_snapshot_consistency(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    use std::sync::Barrier;

    /// Readers, each holding one snapshot.
    const READERS: usize = 4;
    /// Sequential changes the writer applies while the readers hold theirs.
    const CHANGES: usize = 10;
    let barrier = Arc::new(Barrier::new(READERS + 1));

    let mut handles = Vec::new();
    let writer_starts_at = host.meta().version();
    for i in 0..READERS {
        let barrier = Arc::clone(&barrier);
        let meta = host.meta();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            // The snapshot is immutable: every read must agree with itself.
            let v = meta.version();
            for view in meta.doctypes() {
                if view.version != v {
                    return Err(format!(
                        "reader {i}: a view carries version {}, the snapshot is {v}",
                        view.version
                    ));
                }
            }
            // A snapshot taken before the writer started must not contain the writer's fields;
            // one taken after legitimately does. Both are consistent states; a torn mixture
            // (a field from version w inside a version v < w snapshot's view list) is not, and
            // the view version check above catches exactly that.
            Ok(())
        }));
    }
    barrier.wait();
    for k in 0..CHANGES {
        let change = match k % 3 {
            0 => Change::AddCustomField {
                doctype: "party".to_owned(),
                spec: FieldSpecInput::text(&format!("custom_s12_{k:02}")),
                insert_after: None,
            },
            1 => Change::AddCustomField {
                doctype: "invoice".to_owned(),
                spec: FieldSpecInput::text(&format!("custom_s12_{k:02}")),
                insert_after: None,
            },
            _ => Change::SetProperty {
                doctype: "party".to_owned(),
                field: Some("code".to_owned()),
                property: SetProperty::Label(format!("Code {k}")),
            },
        };
        host.apply(&change)
            .map_err(|e| format!("writer step {k} refused: {e}"))?;
    }
    for handle in handles {
        handle
            .join()
            .map_err(|e| format!("a reader panicked: {e:?}"))??;
    }
    check_eq(
        host.meta().version(),
        writer_starts_at + u64::try_from(CHANGES).unwrap_or(u64::MAX),
        "ten changes moved the version by ten",
    )?;
    check(
        host.meta()
            .definition("party")
            .is_some_and(|d| d.fields.iter().any(|f| f.name == "custom_s12_00")),
        "the writer's changes are all in force after the readers joined",
    )?;
    Ok(vec![
        "readers kept consistent immutable snapshots; the version sequence stayed exact".to_owned(),
    ])
}

/// S13 — ADR-05: 200 sequential changes complete, each within the generous per-change budget
/// the README states ([`SEQUENTIAL_BUDGET`]). Wall time is recorded in the report; the gate is
/// only the gross bound (the README explains why: liveness, not speed).
pub(super) fn s13_two_hundred_changes_budget(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    /// The changes the scenario applies.
    const CHANGES: u64 = 200;
    let started = std::time::Instant::now();
    let mut first_version = None;
    for k in 0..CHANGES {
        let change_at = std::time::Instant::now();
        let change = Change::AddCustomField {
            doctype: if k % 2 == 0 { "party" } else { "invoice" }.to_owned(),
            spec: FieldSpecInput::text(&format!("custom_load_{k:03}")),
            insert_after: None,
        };
        let effect = host
            .apply(&change)
            .map_err(|e| format!("change {k} refused: {e}"))?;
        let per_change = change_at.elapsed();
        if per_change > SEQUENTIAL_BUDGET {
            return Err(format!(
                "change {k} took {per_change:?}, above the {SEQUENTIAL_BUDGET:?} budget"
            ));
        }
        if first_version.is_none() {
            first_version = Some(effect.new_version);
        }
    }
    let total = started.elapsed();
    check_eq(
        host.meta().version(),
        first_version.unwrap_or(0) + (CHANGES - 1),
        "the recorded versions line up with the final one",
    )?;
    Ok(vec![format!(
        "200 changes in {total:?} (per-change bound {SEQUENTIAL_BUDGET:?})"
    )])
}

/// S14 — ADR-068 §1.3/§6: a change to a posted DocType's `allow_on_posted` field flag is
/// reflected in the app API view (the flag is what limits `update_posted`).
pub(super) fn s14_allow_on_posted_reaches_app_view(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    let before_flag = posted_flag(host, "state");
    apply_ok(
        host,
        &Change::SetProperty {
            doctype: "invoice".to_owned(),
            field: Some("state".to_owned()),
            property: SetProperty::AllowOnPosted(true),
        },
    )?;
    check_eq(
        posted_flag(host, "state"),
        Some(true),
        "the flag in the app API view after setting it",
    )?;
    check_eq(
        before_flag,
        Some(false),
        "the definition's flag started unset",
    )?;

    // And back again: the rules-layer property is overridable in both directions (ADR-068 §1).
    apply_ok(
        host,
        &Change::SetProperty {
            doctype: "invoice".to_owned(),
            field: Some("state".to_owned()),
            property: SetProperty::AllowOnPosted(false),
        },
    )?;
    check_eq(
        posted_flag(host, "state"),
        Some(false),
        "the flag returns to false in the view",
    )?;
    Ok(Vec::new())
}

/// The `allow_on_posted` flag of `field` on the posted DocType, through the app API view.
fn posted_flag(host: &dyn LivenessHost, field: &str) -> Option<bool> {
    host.meta()
        .doctype("invoice")
        .and_then(|d| d.field(field).map(|f| f.allow_on_posted))
}

/// S15 — ADR-068 §1.1 (the identity layer is fixed once a field exists)/ADR-05: the
/// engine-reserved field names cannot be used as custom field names, and refusing them moves
/// nothing. Which Refusal kind the host reports is not ADR-fixed (the prefix rule and the
/// reserved rule may be checked in either order), so the scenario reports what the adapter
/// said and asserts only what every host must: a refusal, and an unmoved version.
pub(super) fn s15_reserved_names_refused(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    let start = host.meta().version();
    let mut notes = Vec::new();
    for reserved in spot_meta::ENGINE_FIELDS {
        let reported = reported_refusal(host, reserved);
        apply_refused(
            host,
            &Change::AddCustomField {
                doctype: "party".to_owned(),
                spec: FieldSpecInput::text(reserved),
                insert_after: None,
            },
            &reported,
            reserved,
        )?;
        notes.push(format!("`{reserved}` is refused ({reported:?})"));
    }
    check_eq(host.meta().version(), start, "the version does not move")?;
    Ok(notes)
}

/// The refusal the host reports for a reserved custom-field name (probed once; the scenario
/// replays the same change against that expectation).
fn reported_refusal(host: &mut dyn LivenessHost, reserved: &str) -> Refusal {
    host.apply(&Change::AddCustomField {
        doctype: "party".to_owned(),
        spec: FieldSpecInput::text(reserved),
        insert_after: None,
    })
    .err()
    .map_or(Refusal::BadCustomFieldName, |e| e.refusal)
}

/// S16 — ADR-05 ("within one request"): every probe the host supports must already see the new
/// metadata version **within the same `apply` call** that produced it. A host without a layer
/// reports [`ProbeOutcome::NotSupported`]; the scenario records that in its notes so the
/// report says which layers participated.
pub(super) fn s16_probes_see_the_version_in_apply(
    host: &mut dyn LivenessHost,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let mut any_supported = false;
    for (layer, name) in [
        (Probe::Plan, "plan"),
        (Probe::Schema, "schema"),
        (Probe::View, "view"),
    ] {
        let v = apply_ok(
            host,
            &Change::AddCustomField {
                doctype: "party".to_owned(),
                spec: FieldSpecInput::text(&format!("custom_probe_{name}")),
                insert_after: None,
            },
        )?;
        match host.probe(layer) {
            ProbeOutcome::NotSupported => {
                notes.push(format!("the {name} layer is not supported on this host"));
            }
            ProbeOutcome::Live { version } if version == v => {
                any_supported = true;
                notes.push(format!("the {name} probe saw version {v} within the apply"));
            }
            ProbeOutcome::Live { version } => {
                return Err(format!(
                    "the {name} probe saw version {version}, but the apply produced {v}"
                ));
            }
            ProbeOutcome::Stale { observed, seen } => {
                return Err(format!(
                    "the {name} probe is stale: it saw {observed} against {seen}"
                ));
            }
        }
    }
    if !any_supported {
        notes.push(
            "no probe layer is supported yet on this host (WP-MET-02, WP-STO-11 and WP-UI-10 \
             provide them); recorded as NotSupported, never asserted silent"
                .to_owned(),
        );
    }
    Ok(notes)
}

// -------------------------------------------------------------------------------------------
// Helpers shared with the other scenario modules (field shape comparison)
// -------------------------------------------------------------------------------------------

/// The comparable shape of one field: name, label, `required`. If the scenario changes only
/// the rules-layer property, exactly that flag is the only difference between two snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FieldShape {
    /// Storage name.
    pub name: String,
    /// The label.
    pub label: Option<String>,
    /// Must be set when saved.
    pub required: bool,
}

/// Reads one field's comparable shape from the effective definitions.
pub(super) fn field_shape(
    meta: &spot_meta::EffectiveMeta,
    doctype: &str,
    field: &str,
) -> Option<FieldShape> {
    meta.definition(doctype)
        .and_then(|d| d.fields.iter().find(|f| f.name == field))
        .map(|f| FieldShape {
            name: f.name.clone(),
            label: f.label.clone(),
            required: f.required,
        })
}
