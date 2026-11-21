//! The merge applies layers from least to most specific, then listed order within a rank;
//! diffs that no longer fit drop with warnings, never errors.

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_views::layer::{LayerKind, LayerName, Op, ViewLayer};
use spot_views::resolve::{ResolvedView, resolve, resolve_over};
use spot_views::viewdef::{Item, Section, ViewDef, ViewKind};

/// A form view over the given top-level sections with the given field order.
fn base(doctype: &str, section_id: &str, fields: &[&str]) -> ViewDef {
    ViewDef {
        doctype: doctype.to_owned(),
        kind: ViewKind::Form,
        role: None,
        title: None,
        density: None,
        tabs: vec![],
        sections: vec![Section {
            id: section_id.to_owned(),
            title: None,
            columns: None,
            items: fields
                .iter()
                .map(|field| Item::Field {
                    field: (*field).to_owned(),
                    label: None,
                    control: None,
                    lookup: None,
                })
                .collect(),
        }],
        columns: vec![],
        sort: None,
        actions: vec![],
    }
}

/// An effective metadata over one DocType with the given fields, in order.
fn meta(doctype: &str, fields: &[&str]) -> std::sync::Arc<spot_meta::EffectiveMeta> {
    let mut definition = Definition::new(doctype);
    definition.fields = fields
        .iter()
        .map(|field| FieldSpec::new(field, FieldType::Text))
        .collect();
    Registry::new(vec![definition])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("test metadata is invalid: {errors:?}"))
}

/// A layer of `kind` with the given ops.
fn layer(doctype: &str, kind: LayerKind, ops: Vec<Op>) -> ViewLayer {
    ViewLayer {
        doctype: doctype.to_owned(),
        kind,
        ops,
    }
}

/// A move of `field`: after `anchor` in its section, or into `section` when given.
fn move_field(field: &str, section: Option<&str>, anchor: Option<&str>) -> Op {
    Op::MoveField {
        field: field.to_owned(),
        tab: None,
        section: section.map(str::to_owned),
        before: None,
        after: anchor.map(str::to_owned),
    }
}

/// The field order the resolved form shows.
fn order(resolved: &ResolvedView) -> Vec<String> {
    resolved
        .view
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn specific_rank_wins_over_listed_order() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let base = base("invoice", "general", &["customer", "date", "total"]);
    // The step layer is listed first but applies last (most specific rank).
    let step = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Step,
            step: Some("approve".to_owned()),
            role: None,
        },
        vec![move_field("date", None, Some("total"))],
    );
    let site = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![move_field("total", None, Some("customer"))],
    );
    let resolved = resolve(&base, &[step, site], &meta);
    // Site applies first, then the step: customer, total, date.
    assert_eq!(order(&resolved), vec!["customer", "total", "date"]);
    assert!(resolved.warnings.is_empty(), "{resolved:?}");
}

#[test]
fn listed_order_decides_within_a_rank() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let base = base("invoice", "general", &["customer", "date", "total"]);
    let first = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![move_field("date", None, Some("customer"))],
    );
    let second = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![move_field("date", None, Some("total"))],
    );
    let resolved = resolve(&base, &[first.clone(), second.clone()], &meta);
    assert!(
        resolved.warnings.is_empty(),
        "the moves must not drop: {:?}",
        resolved.warnings
    );
    assert_eq!(order(&resolved), vec!["customer", "total", "date"]);
    // The swapped listing: second runs first (date after total), then first re-places it
    // after the customer — a different, still deterministic, order.
    let swapped = resolve(&base, &[second.clone(), first.clone()], &meta);
    assert_eq!(order(&swapped), vec!["customer", "date", "total"]);
}

#[test]
fn stale_diffs_warn_and_drop_but_never_error() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let base = base("invoice", "general", &["customer", "date", "total"]);
    let site = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![
            move_field("customer", Some("archive"), None),
            Op::HideField {
                field: "legacy".to_owned(),
            },
            Op::RenameSection {
                section: "archive".to_owned(),
                title: "Old".to_owned(),
            },
            Op::MoveField {
                field: "date".to_owned(),
                tab: Some("missing_tab".to_owned()),
                section: None,
                before: None,
                after: None,
            },
        ],
    );
    let resolved = resolve(&base, &[site], &meta);
    assert_eq!(
        order(&resolved),
        vec!["customer", "date", "total"],
        "a failed move keeps the field"
    );
    let codes: Vec<&str> = resolved
        .warnings
        .iter()
        .map(|warning| warning.code)
        .collect();
    assert_eq!(
        codes,
        vec![
            "view.unknown_section",
            "view.unknown_field",
            "view.unknown_section",
            "view.unknown_tab"
        ],
        "{resolved:?}"
    );
}

#[test]
fn a_step_context_applies_over_a_resolved_view() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let base = base("invoice", "general", &["customer", "date", "total"]);
    let site = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![move_field("total", None, Some("customer"))],
    );
    let resolved = resolve(&base, &[site], &meta);
    assert_eq!(order(&resolved), vec!["customer", "total", "date"]);
    let step = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Step,
            step: Some("approve".to_owned()),
            role: None,
        },
        vec![Op::HideField {
            field: "total".to_owned(),
        }],
    );
    let with_step = resolve_over(&resolved, &step, &meta);
    assert_eq!(order(&with_step), vec!["customer", "date"]);
    assert_eq!(with_step.hidden, vec!["total"]);
}

#[test]
fn an_anchor_that_vanished_falls_back_to_the_section_end() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let base = base("invoice", "general", &["customer", "date", "total"]);
    let site = layer(
        "invoice",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![move_field("total", Some("general"), Some("archive_anchor"))],
    );
    let resolved = resolve(&base, &[site], &meta);
    // The anchor is gone, so the field lands at the end of the section — never lost.
    assert_eq!(order(&resolved), vec!["customer", "date", "total"]);
    let codes: Vec<&str> = resolved
        .warnings
        .iter()
        .map(|warning| warning.code)
        .collect();
    assert_eq!(codes, vec!["view.unknown_anchor"], "{resolved:?}");
}

#[test]
fn a_layer_for_another_doctype_is_skipped_with_a_warning() {
    let meta = meta("invoice", &["customer", "date"]);
    let base = base("invoice", "general", &["customer", "date"]);
    let other = layer(
        "order",
        LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        vec![move_field("customer", Some("general"), None)],
    );
    let resolved = resolve(&base, &[other], &meta);
    assert_eq!(order(&resolved), vec!["customer", "date"]);
    assert_eq!(resolved.warnings.len(), 1);
    assert_eq!(resolved.warnings[0].code, "view.bad_shape");
}
