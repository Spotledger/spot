//! Moves, hides and shows round trip: a hidden field leaves the view, a shown one re-enters
//! at the default position (definition order, appended), and moves land where asked.

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_views::layer::{LayerKind, LayerName, Op, ViewLayer};
use spot_views::resolve::resolve;
use spot_views::viewdef::{Item, Section, Tab, ViewDef, ViewKind};

fn base(doctype: &str, fields: &[&str]) -> ViewDef {
    ViewDef {
        doctype: doctype.to_owned(),
        kind: ViewKind::Form,
        role: None,
        title: None,
        density: None,
        tabs: vec![Tab {
            id: "main".to_owned(),
            title: Some("Main".to_owned()),
            sections: vec![Section {
                id: "general".to_owned(),
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
        }],
        sections: vec![],
        columns: vec![],
        sort: None,
        actions: vec![],
    }
}

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

fn order(resolved: &spot_views::resolve::ResolvedView) -> Vec<String> {
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
fn move_between_tabs_and_sections() {
    let meta = meta("invoice", &["customer", "date", "total", "note"]);
    let mut view = base("invoice", &["customer", "date", "total", "note"]);
    view.tabs.push(Tab {
        id: "totals".to_owned(),
        title: Some("Totals".to_owned()),
        sections: vec![Section {
            id: "totals_grid".to_owned(),
            title: None,
            columns: None,
            items: vec![Item::Control {
                control: "heading".to_owned(),
                text: Some("Totals".to_owned()),
            }],
        }],
    });
    let site = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![
            Op::MoveField {
                field: "total".to_owned(),
                tab: None,
                section: Some("totals_grid".to_owned()),
                before: None,
                after: None,
            },
            Op::MoveField {
                field: "note".to_owned(),
                tab: None,
                section: Some("general".to_owned()),
                before: Some("date".to_owned()),
                after: None,
            },
        ],
    };
    let resolved = resolve(&view, &[site], &meta);
    assert_eq!(order(&resolved), vec!["customer", "note", "date", "total"]);
    // The moved field sits after the heading in the totals tab's section.
    let totals = match resolved.view.tabs.iter().find(|tab| tab.id == "totals") {
        Some(tab) => &tab.sections[0],
        None => panic!("the totals tab is present"),
    };
    assert!(matches!(&totals.items[0], Item::Control { control, .. } if control == "heading"));
    assert!(matches!(&totals.items[1], Item::Field { field, .. } if field == "total"));
}

#[test]
fn hide_then_show_round_trips_again() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let view = base("invoice", &["customer", "date", "total"]);
    // Hiding a field the view does not even carry would warn; here the hide is real.
    let hide_total = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![Op::HideField {
            field: "total".to_owned(),
        }],
    };
    let show_total = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![Op::ShowField {
            field: "total".to_owned(),
        }],
    };
    let hidden = resolve(&view, std::slice::from_ref(&hide_total), &meta);
    assert_eq!(order(&hidden), vec!["customer", "date"]);
    assert_eq!(hidden.hidden, vec!["total"]);
    // A later layer shows it again: it re-enters at the default position (appended last).
    let shown = resolve(&view, &[hide_total, show_total], &meta);
    assert_eq!(order(&shown), vec!["customer", "date", "total"]);
    assert!(shown.hidden.is_empty());
}

#[test]
fn an_unmentioned_hidden_field_is_excluded_from_the_append() {
    let meta = meta("invoice", &["customer", "date", "total", "note"]);
    let view = base("invoice", &["customer", "date"]);
    let site = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![
            Op::HideField {
                field: "total".to_owned(),
            },
            // `note` stays unmentioned and unhidden: it must still appear (appended).
            Op::AddItem {
                section: "general".to_owned(),
                anchor: None,
                item: Item::Control {
                    control: "divider".to_owned(),
                    text: None,
                },
            },
        ],
    };
    let resolved = resolve(&view, &[site], &meta);
    assert_eq!(order(&resolved), vec!["customer", "date", "note"]);
    assert_eq!(resolved.hidden, vec!["total"]);
}
