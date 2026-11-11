//! Which diffs commute — documented here, pinned in code, per the brief.
//!
//! **Commute** (layers of equal kind may be listed in either order, same result):
//! - `hide_field`/`show_field` over *different* fields;
//! - `rename_section` on a section nobody else's diff *adds* (both reference the same
//!   section id by id — ids never change);
//! - `add_item` into a section another op does not move or remove items of;
//! - `move_field` into a *different* section than the other op's subject (or anchored
//!   differently within the same section);
//! - `set_hint` on a different field, and `set_columns` over anything (form ops of equal
//!   rank) — different kind gates entirely.
//!
//! **Do not commute** (the listed order decides, by design):
//! - two unanchored `move_field`s into the same section (they land in op order);
//! - two `set_hint`s on the same field (the later hint wins);
//! - `hide_field` then `show_field` on the same field (and the reverse: show then hide
//!   ends hidden — the later op wins, whichever it is).

use spot_views::layer::{LayerKind, LayerName, Op, ViewLayer};
use spot_views::resolve::{ResolvedView, resolve};

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_views::viewdef::{Item, Section, ViewDef, ViewKind};

fn meta() -> std::sync::Arc<spot_meta::EffectiveMeta> {
    let mut definition = Definition::new("invoice");
    definition.fields = vec![
        FieldSpec::new("customer", FieldType::Text),
        FieldSpec::new("date", FieldType::Date),
        FieldSpec::new("total", FieldType::Text),
    ];
    Registry::new(vec![definition])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("test metadata is invalid: {errors:?}"))
}

fn base() -> ViewDef {
    ViewDef {
        doctype: "invoice".to_owned(),
        kind: ViewKind::Form,
        role: None,
        title: None,
        density: None,
        tabs: vec![],
        sections: vec![
            Section {
                id: "general".to_owned(),
                title: None,
                columns: None,
                items: vec![
                    Item::Field {
                        field: "customer".to_owned(),
                        label: None,
                        control: None,
                        lookup: None,
                    },
                    Item::Field {
                        field: "date".to_owned(),
                        label: None,
                        control: None,
                        lookup: None,
                    },
                ],
            },
            Section {
                id: "totals".to_owned(),
                title: None,
                columns: None,
                items: vec![Item::Field {
                    field: "total".to_owned(),
                    label: None,
                    control: None,
                    lookup: None,
                }],
            },
        ],
        columns: vec![],
        sort: None,
        actions: vec![],
    }
}

fn site(ops: Vec<Op>) -> ViewLayer {
    ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops,
    }
}

fn run(first: ViewLayer, second: ViewLayer) -> ResolvedView {
    resolve(&base(), &[first, second], &meta())
}

fn field_order(resolved: &ResolvedView) -> Vec<String> {
    resolved
        .view
        .all_sections()
        .into_iter()
        .flat_map(|section| section.items.iter())
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn independent_ops_commute() {
    let meta = meta();
    let a = site(vec![
        Op::SetHint {
            field: "date".to_owned(),
            label: Some("Invoice date".to_owned()),
            control: None,
            lookup: None,
        },
        Op::RenameSection {
            section: "totals".to_owned(),
            title: "Money".to_owned(),
        },
    ]);
    let b = site(vec![
        Op::MoveField {
            field: "total".to_owned(),
            tab: None,
            section: Some("totals".to_owned()),
            before: None,
            after: None,
        },
        Op::SetHint {
            field: "customer".to_owned(),
            label: Some("Client".to_owned()),
            control: None,
            lookup: None,
        },
    ]);
    let ab = resolve(&base(), &[a.clone(), b.clone()], &meta);
    let ba = resolve(&base(), &[b, a], &meta);
    assert_eq!(field_order(&ab), field_order(&ba));
    assert_eq!(ab.hidden, ba.hidden);
    assert_eq!(
        ab.view, ba.view,
        "the renamed title and the hint land either way"
    );
    assert_eq!(ab.warnings.len(), ba.warnings.len());
}

#[test]
fn two_moves_into_one_section_keep_listed_order() {
    let a = site(vec![Op::MoveField {
        field: "customer".to_owned(),
        tab: None,
        section: Some("totals".to_owned()),
        before: None,
        after: None,
    }]);
    let b = site(vec![Op::MoveField {
        field: "date".to_owned(),
        tab: None,
        section: Some("totals".to_owned()),
        before: None,
        after: None,
    }]);
    // Unanchored moves append at the section end in op order: [customer] then [date].
    let ab = run(a.clone(), b.clone());
    assert_eq!(field_order(&ab), vec!["total", "customer", "date"]);
    let ba = run(b, a);
    assert_eq!(field_order(&ba), vec!["total", "date", "customer"]);
}

#[test]
fn hide_then_show_keeps_the_later_op() {
    let hide = site(vec![Op::HideField {
        field: "total".to_owned(),
    }]);
    let show = site(vec![Op::ShowField {
        field: "total".to_owned(),
    }]);
    let hidden = run(hide.clone(), show.clone());
    assert_eq!(
        field_order(&hidden),
        vec!["customer", "date", "total"],
        "show wins: re-enters at end"
    );
    assert!(hidden.hidden.is_empty());
    let shown = run(show, hide);
    assert_eq!(field_order(&shown), vec!["customer", "date"]);
    assert_eq!(shown.hidden, vec!["total"]);
}
