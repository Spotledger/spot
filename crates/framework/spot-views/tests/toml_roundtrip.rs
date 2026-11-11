//! TOML in and out: golden texts, byte-stable key order, exact positions, and a round-trip
//! property over generated views.

use proptest::prelude::*;
use spot_views::viewdef::{Action, Column, Density, Item, Section, Sort, Tab, ViewDef, ViewKind};

const INVOICE_TOML: &str = r#"
doctype = "invoice"
kind = "form"
title = "Invoice"
density = "comfortable"

[[tabs]]
id = "totals"
title = "Totals"
[[tabs.sections]]
id = "totals_grid"
title = "Lines"
columns = 2
[[tabs.sections.items]]
type = "field"
field = "invoice_items"
[[tabs.sections.items]]
type = "control"
control = "help"
text = "Rows bill one line each."

[[tabs]]
id = "general_tab"
title = "General"
[[tabs.sections]]
id = "general"
[[tabs.sections.items]]
type = "field"
field = "customer"
label = "Customer"
control = "lookup_box"
lookup = "platform:link_search"
[[tabs.sections.items]]
type = "control"
control = "divider"
[[tabs.sections.items]]
type = "group"
id = "who"
items = [{ type = "field", field = "date" }]

[[actions]]
name = "post"
label = "Post"
when = "total > 0"
"#;

#[test]
fn the_golden_text_parses_and_writes_back_the_same() {
    let view = match ViewDef::parse("invoice.view.toml", INVOICE_TOML) {
        Ok(view) => view,
        Err(errors) => panic!("the golden view must parse: {errors:?}"),
    };
    assert_eq!(view.doctype, "invoice");
    assert_eq!(view.kind, ViewKind::Form);
    assert_eq!(view.density, Some(Density::Comfortable));
    assert_eq!(view.tabs.len(), 2);
    assert_eq!(view.tabs[0].sections[0].columns, Some(2));
    assert_eq!(view.tabs[1].sections[0].id, "general");
    assert_eq!(view.sections.len(), 0);
    assert_eq!(
        view.actions,
        vec![Action {
            name: "post".to_owned(),
            label: "Post".to_owned(),
            when: Some("total > 0".to_owned()),
        }]
    );
    // The group nests.
    let group = match &view.tabs[1].sections[0].items[2] {
        Item::Group(group) => group,
        other => panic!("the third item is the group: {other:?}"),
    };
    assert_eq!(group.id, "who");
    assert_eq!(group.items.len(), 1);

    // Round trip: parse (write ( parse ( text ))) equals parse ( text ), byte for byte.
    let written = match view.to_toml() {
        Ok(text) => text,
        Err(e) => panic!("cannot write: {e}"),
    };
    let reparsed = match ViewDef::parse("invoice.view.toml", &written) {
        Ok(view) => view,
        Err(errors) => panic!("the written view must parse: {errors:?}"),
    };
    assert_eq!(reparsed, view);
    let written_twice = match reparsed.to_toml() {
        Ok(text) => text,
        Err(e) => panic!("cannot write: {e}"),
    };
    assert_eq!(written, written_twice, "the key order is stable");
}

#[test]
fn a_mistyped_key_is_a_parse_error_with_a_position() {
    let text = "doctype = \"invoice\"\nkind = \"form\"\nwrong_key = 1\n[[sections]]\nid = \"general\"\nitems = []\n";
    let errors = match ViewDef::parse("bad.view.toml", text) {
        Ok(_) => panic!("an unknown key must fail"),
        Err(errors) => errors,
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind.code(), "view.parse");
    assert!(
        errors[0].message.contains("unknown field `wrong_key`"),
        "{}",
        errors[0].message
    );
    let position = match errors[0].position {
        Some(position) => position,
        None => panic!("the error carries a position"),
    };
    assert_eq!(
        (position.line, position.column),
        (3, 1),
        "{:?}",
        errors[0].position
    );
    assert_eq!(errors[0].file.as_deref(), Some("bad.view.toml"));
}

#[test]
fn semantic_problems_report_all_at_once() {
    let text = concat!(
        "doctype = \"invoice\"\n",
        "kind = \"form\"\n",
        "[[sections]]\n",
        "id = \"General\"\n",
        "items = []\n",
        "[[sections]]\n",
        "id = \"general\"\n",
        "items = []\n",
        "[[sections]]\n",
        "id = \"general\"\n",
        "items = [{ type = \"control\", control = \"carousel\" }]\n",
    );
    let errors = match ViewDef::parse("bad.view.toml", text) {
        Ok(_) => panic!("the problems must fail"),
        Err(errors) => errors,
    };
    let codes: Vec<&'static str> = errors.iter().map(|error| error.kind.code()).collect();
    assert_eq!(
        codes,
        vec!["view.bad_id", "view.duplicate", "view.unknown_control"],
        "all three problems, once each, in report order: {errors:?}"
    );
}

#[test]
fn position_of_maps_offsets() {
    let text = "a\nbb\nccc";
    let at = spot_views::error::position_of(text, 0);
    assert_eq!((at.line, at.column), (1, 1));
    let at = spot_views::error::position_of(text, 2);
    assert_eq!((at.line, at.column), (2, 1));
    let at = spot_views::error::position_of(text, 4);
    assert_eq!((at.line, at.column), (2, 3));
}

proptest! {
    /// parse(write(v)) == v for generated views: the wire keeps its meaning.
    #[test]
    fn views_round_trip_through_toml(
        doctype in "[a-z][a-z0-9_]{0,30}",
        field_a in "[a-z][a-z0-9_]{0,20}",
        section in "[a-z][a-z0-9_]{0,20}",
        label in proptest::option::of("[a-zA-Z0-9 ]{0,40}"),
        density in proptest::option::of(proptest::prop_oneof![
            Just(Density::Compact),
            Just(Density::Comfortable),
            Just(Density::Spacious),
        ]),
        columns in 0..4u8,
        kind in proptest::prop_oneof![Just(ViewKind::Form), Just(ViewKind::List)],
    ) {
        let is_form = kind == ViewKind::Form;
        let view = ViewDef {
            doctype: doctype.clone(),
            kind,
            role: None,
            title: None,
            density, // a form-only hint; `density` on a list is not a shape error today
            tabs: if is_form {
                vec![Tab {
                    id: "main".to_owned(),
                    title: None,
                    sections: vec![Section {
                        id: section.clone(),
                        title: label.clone(),
                        columns: Some(columns),
                        items: vec![
                            Item::Field {
                                field: field_a.clone(),
                                label: label.clone(),
                                control: None,
                                lookup: None,
                            },
                            Item::Control {
                                control: "spacer".to_owned(),
                                text: None,
                            },
                        ],
                    }],
                }]
            } else {
                vec![]
            },
            sections: vec![],
            columns: if is_form {
                vec![]
            } else {
                vec![Column { field: field_a.clone(), label: None }]
            },
            sort: if is_form {
                None
            } else {
                Some(Sort { field: field_a.clone(), descending: false })
            },
            actions: vec![],
        };
        let wire = view.to_toml().map_err(|e| TestCaseError::fail(e.to_string()))?;
        let back = ViewDef::parse("prop.view.toml", &wire)
            .map_err(|errors| TestCaseError::fail(format!("{errors:?}")))?;
        prop_assert_eq!(back, view);
    }

    /// The layer round trips too, ops included.
    #[test]
    fn layers_round_trip_through_toml(
        doctype in "[a-z][a-z0-9_]{0,30}",
        field in "[a-z][a-z0-9_]{0,20}",
        role in "[a-z][a-z0-9_]{0,20}",
    ) {
        let layer = spot_views::ViewLayer {
            doctype: doctype.clone(),
            kind: spot_views::LayerKind {
                kind: spot_views::LayerName::Role,
                role: Some(role.clone()),
                step: None,
            },
            ops: vec![
                spot_views::Op::HideField { field: field.clone() },
                spot_views::Op::ShowField { field: field.clone() },
                spot_views::Op::SetHint {
                    field: field.clone(),
                    label: Some("Client".to_owned()),
                    control: None,
                    lookup: Some("platform:link_search".to_owned()),
                },
            ],
        };
        let wire = layer.to_toml().map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert!(wire.contains("kind = \"role\""), "{wire}");
        let back = spot_views::ViewLayer::parse("prop.layer.toml", &wire)
            .map_err(|errors| TestCaseError::fail(format!("{errors:?}")))?;
        prop_assert_eq!(back, layer);
    }
}
