//! Preview is a sandbox view: it writes nothing and changes nothing.

mod support;

use spot_meta::{Definition, FieldSpec, FieldType};
use spot_nocode_model::{Draft, DraftOp, SiteContext, apply_op, preview, preview_in, sample_value};
use support::*;

/// A small fixed draft: one DocType with one text field, built by ops only.
fn small_draft() -> Draft {
    apply_op(
        &Draft::new("draft-ticket", "ann", "ticket", "Ticket"),
        &DraftOp::AddField {
            child: None,
            name: "subject".to_owned(),
            label: Some("Subject".to_owned()),
            ty: FieldType::Text,
            insert_after: None,
        },
    )
    .unwrap_or_else(|e| panic!("the small draft's op applies: {e}"))
}

/// A site holding the `vehicle` DocType plus an unrelated installed one, in the given order.
fn site_with(vehicle_first: bool) -> SiteContext {
    let mut warehouse = Definition::new("warehouse");
    warehouse.label = Some("Warehouse".to_owned());
    warehouse.fields = vec![FieldSpec::new("code", FieldType::Text)];
    let definitions = if vehicle_first {
        vec![vehicle_definition(), warehouse]
    } else {
        vec![warehouse, vehicle_definition()]
    };
    SiteContext::new(site_id()).with_definitions(definitions)
}

#[test]
fn preview_leaves_the_draft_untouched() {
    let editor = build_vehicle_service_request();
    let before = editor.current().clone();
    let site = site_with_vehicle();
    let preview = preview_in(editor.current(), &site);
    // The draft is byte-for-byte unchanged and the preview says it is a sandbox.
    assert_eq!(editor.current(), &before);
    assert!(preview.sandbox);
    assert!(preview.valid);
    // The preview carries the real form and the generated list view.
    assert_eq!(preview.form.doctype, "vehicle_service_request");
    assert!(preview.list.is_some());
    assert_eq!(preview.children.len(), 1);
}

#[test]
fn preview_without_the_site_reports_the_unresolved_link() {
    let editor = build_vehicle_service_request();
    // `vehicle` is an installed DocType the draft does not define; without the site's
    // definitions the link cannot be resolved, and the preview says so rather than guessing.
    let preview = preview(&editor.current().clone());
    assert!(!preview.valid);
    assert!(!preview.issues.is_empty());
}

#[test]
fn sample_values_are_deterministic_and_by_type() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let preview = preview_in(editor.current(), &site);
    let by_name = |name: &str| {
        preview
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} is previewed"))
            .sample
            .clone()
    };
    assert_eq!(by_name("request_date"), "2026-01-01");
    assert_eq!(by_name("priority"), "Low");
    assert_eq!(by_name("vehicle"), "vehicle");
    // Two previews of the same draft are equal.
    let again = preview_in(editor.current(), &site);
    assert_eq!(preview, again);
}

#[test]
fn sample_value_covers_every_type() {
    use spot_meta::{FieldSpec, FieldType};
    assert_eq!(sample_value(&FieldSpec::new("b", FieldType::Bool)), "true");
    assert_eq!(sample_value(&FieldSpec::new("i", FieldType::Int)), "1");
    assert_eq!(
        sample_value(&FieldSpec::new("d", FieldType::Decimal)),
        "0.00"
    );
    assert_eq!(
        sample_value(&FieldSpec::new("s", FieldType::Secret)),
        "(hidden)"
    );
}

#[test]
fn the_digest_is_64_lowercase_hex() {
    let preview = preview(&small_draft());
    assert_eq!(preview.digest.len(), 64, "digest: {}", preview.digest);
    assert!(
        preview
            .digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "digest is lower-case hex: {}",
        preview.digest
    );
}

#[test]
fn the_same_inputs_give_the_same_digest_in_any_site_order() {
    let editor = build_vehicle_service_request();
    // The same draft and the same site definitions: the same digest, twice.
    let first = preview_in(editor.current(), &site_with_vehicle());
    let second = preview_in(editor.current(), &site_with_vehicle());
    assert_eq!(first.digest, second.digest);
    // The site's definitions passed in the other order: still the same digest, because the
    // canonical form sorts them by name.
    assert_eq!(site_with(true).definitions().len(), 2);
    assert_ne!(
        site_with(true).definitions()[0].name,
        site_with(false).definitions()[0].name,
        "the two sites really are in a different order"
    );
    let forward = preview_in(editor.current(), &site_with(true));
    let reverse = preview_in(editor.current(), &site_with(false));
    assert_eq!(forward.digest, reverse.digest);
    assert!(forward.valid && reverse.valid);
}

#[test]
fn a_changed_site_definition_changes_the_digest() {
    let editor = build_vehicle_service_request();
    let before = preview_in(editor.current(), &site_with_vehicle()).digest;
    // The draft links to `vehicle`; a new field on the installed `vehicle` is an input change.
    let mut changed = vehicle_definition();
    changed
        .fields
        .push(FieldSpec::new("mileage", FieldType::Int));
    let site = SiteContext::new(site_id()).with_definition(changed);
    let after = preview_in(editor.current(), &site).digest;
    assert_ne!(before, after);
}

#[test]
fn a_changed_field_in_the_draft_changes_the_digest() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let before = preview_in(editor.current(), &site).digest;
    // One field becomes required: the definitions the preview was compiled from changed.
    let mut editor = editor;
    apply(
        &mut editor,
        DraftOp::SetRequired {
            child: None,
            name: "request_date".to_owned(),
            required: true,
        },
    );
    let after = preview_in(editor.current(), &site).digest;
    assert_ne!(before, after);
}

#[test]
fn two_different_drafts_differ() {
    let site = site_with_vehicle();
    let one = preview_in(&small_draft(), &site).digest;
    let other = preview_in(build_vehicle_service_request().current(), &site).digest;
    assert_ne!(one, other);
    // A second copy of the same small draft is the same preview, not a new one.
    assert_eq!(one, preview_in(&small_draft(), &site).digest);
}

#[test]
fn the_error_path_has_a_digest_that_changes_with_the_draft() {
    // Without the site's definitions the draft's `vehicle` link does not resolve, so the
    // preview is an error list — and it is still keyed by the inputs it was built from.
    let editor = build_vehicle_service_request();
    let broken = preview(editor.current());
    assert!(!broken.valid);
    assert_eq!(broken.digest.len(), 64);
    assert!(
        broken
            .digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    // A changed field on the same broken draft moves the digest.
    let mut changed = editor.current().clone();
    changed
        .doctype_def
        .fields
        .push(FieldSpec::new("extra", FieldType::Text));
    let after = preview(&changed);
    assert!(!after.valid);
    assert_ne!(broken.digest, after.digest);
}

#[test]
fn the_golden_digest_is_pinned() {
    // One small fixed draft with no site definitions, so an accidental change to the canonical
    // form (a separator, the sort, the serialisation) is seen rather than silently shipped.
    let preview = preview(&small_draft());
    assert!(preview.valid, "preview issues: {:?}", preview.issues);
    assert_eq!(
        preview.digest,
        "18fcdfd423f61d6756e23e8a1518c0c8b40565e47b1e20b2ac040df341570e16"
    );
}
