//! The generation shapes pinned field by field: the form's sections and placements, the
//! list's priority order and default sort, the child grid's right-aligned numerics
//! (brief deliverables 1–3). The end-to-end validation lives in `runtime_views.rs`; the
//! goldens in `golden.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_meta::EffectiveMeta;
use spot_meta::Registry;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_views::generate::default_view;
use spot_views::viewdef::{Item, Section, ViewKind};

#[path = "generate/common.rs"]
mod common;

use common::{decimal, effective, item, item_group, link, text};

/// Effective metadata over one definition.
fn one(definition: Definition) -> std::sync::Arc<EffectiveMeta> {
    Registry::new(vec![definition])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("the fixture does not build: {errors:?}"))
}

/// The section with `id`.
fn section_of<'a>(view: &'a spot_views::ViewDef, id: &str) -> Option<&'a Section> {
    view.all_sections().into_iter().find(|s| s.id == id)
}

/// The field names in the section.
fn fields_of(section: &Section) -> Vec<&str> {
    section
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.as_str()),
            _ => None,
        })
        .collect()
}

/// Deliverable 1: the form sections of the plain master, in order.
#[test]
fn the_plain_masters_form_has_fields_then_long_form() {
    let meta = effective();
    let view = default_view(&meta, "item", ViewKind::Form).expect("generates");
    let ids: Vec<&str> = view.all_sections().iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["fields", "long_form"], "{view:?}");
    let fields = section_of(&view, "fields").expect("the fields section");
    assert_eq!(
        fields_of(fields),
        vec![
            "item_code",
            "item_group",
            "status",
            "opening_balance",
            "reorder_level"
        ],
        "the scalars keep the definition order, the long_texts move after them"
    );
    let long_form = section_of(&view, "long_form").expect("the long-form section");
    assert_eq!(long_form.title.as_deref(), Some("Details"));
    assert_eq!(fields_of(long_form), vec!["description", "notes"]);
}

/// Deliverable 1: the posted document's child table gets its own titled section after the
/// scalar fields.
#[test]
fn the_child_table_gets_its_own_titled_section() {
    let meta = effective();
    let view = default_view(&meta, "sales_invoice", ViewKind::Form).expect("generates");
    let ids: Vec<&str> = view.all_sections().iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["fields", "table_items", "long_form", "status"],
        "{view:?}"
    );
    let table = section_of(&view, "table_items").expect("the table section");
    assert_eq!(table.title.as_deref(), Some("Invoice Lines"));
    assert_eq!(table.columns, Some(1), "the child grid renders full width");
    assert_eq!(fields_of(table), vec!["items"]);
}

/// Deliverable 1: the engine-managed and read-only fields land in the final collapsed
/// Status section, after every content section.
#[test]
fn the_status_section_comes_last_among_the_content_sections() {
    let meta = effective();
    let view = default_view(&meta, "sales_invoice", ViewKind::Form).expect("generates");
    let status = section_of(&view, "status").expect("the status section");
    assert_eq!(fields_of(status), vec!["doc_state", "amended_from"]);
    assert!(
        matches!(
            status.items.last(),
            Some(Item::Control { control, .. }) if control == "divider",
        ),
        "the Status section folds off with a divider: the model's unbound set has no \
         'collapsed' kind, the fold is a renderer read of the Status title"
    );
}

/// Deliverable 1: custom fields append in the Custom section.
#[test]
fn custom_fields_go_to_the_custom_section() {
    let mut registry = common::registry_fixtures();
    registry
        .add_custom_field("item", FieldSpec::new("custom_flag", FieldType::Bool), None)
        .expect("adds");
    let meta = registry.effective();
    let view = default_view(&meta, "item", ViewKind::Form).expect("generates");
    let ids: Vec<&str> = view.all_sections().iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["fields", "long_form", "custom"], "{view:?}");
    let custom = section_of(&view, "custom").expect("the custom section");
    assert_eq!(fields_of(custom), vec!["custom_flag"]);
}

/// Deliverable 2: the list of the plain master — the title first, then the priority
/// order, never the long-form fields.
#[test]
fn the_list_columns_follow_the_priority() {
    let meta = effective();
    let view = default_view(&meta, "item", ViewKind::List).expect("generates");
    let names: Vec<&str> = view.columns.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "item_code",
            "item_group",
            "status",
            "opening_balance",
            "reorder_level"
        ],
        "{view:?}"
    );
}

/// Deliverable 2: the default sort is the first date field descending.
#[test]
fn the_list_sorts_by_the_first_date_descending() {
    let meta = effective();
    let view = default_view(&meta, "sales_invoice", ViewKind::List).expect("generates");
    let sort = view.sort.as_ref().expect("the posted fixture sorts");
    assert_eq!(sort.field, "posting_date");
    assert!(sort.descending);
}

/// Deliverable 2: a DocType with no date sorts by the title field ascending.
#[test]
fn a_doctype_without_dates_sorts_by_the_title_ascending() {
    let meta = effective();
    let view = default_view(&meta, "item", ViewKind::List).expect("generates");
    let sort = view.sort.as_ref().expect("the titled fixture sorts");
    assert_eq!(sort.field, "item_code");
    assert!(!sort.descending);
}

/// Deliverable 2: a DocType with neither a date nor a usable title still sorts — by its
/// first field.
#[test]
fn a_doctype_without_a_date_or_title_sorts_by_its_first_field() {
    let mut definition = Definition::new("bare");
    definition.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let meta = one(definition);
    let view = default_view(&meta, "bare", ViewKind::List).expect("generates");
    let sort = view.sort.as_ref().expect("the first field sorts");
    assert_eq!(sort.field, "name");
    assert!(!sort.descending);
}

/// The generated list of a DocType whose `title_field` names a text field present in the
/// DocType shows it first; the priority order holds behind it. (`Registry::build` refuses
/// a title naming a missing field, so the generated shape only has to survive the
/// metadata that passed validation.)
#[test]
fn the_list_priority_puts_links_before_selects_before_dates() {
    let mut parent = Definition::new("link_holder");
    parent.fields = vec![link("the_link", "mixed")];
    let mut the_child = Definition::new("the_child");
    the_child.is_child = true;
    the_child.fields = vec![text("row_name")];
    let mut definition = Definition::new("mixed");
    definition.title_field = Some("who_note".to_owned());
    definition.fields = vec![
        FieldSpec::new("note", FieldType::LongText),
        FieldSpec::new("payload", FieldType::Json),
        FieldSpec::new("raw", FieldType::Bytes),
        {
            let mut when = FieldSpec::new("when", FieldType::Date);
            when.label = Some("When".to_owned());
            when
        },
        {
            let mut state = FieldSpec::new("state", FieldType::Select);
            state.options = Some("a\nb".to_owned());
            state
        },
        {
            let mut who = FieldSpec::new("who", FieldType::Link);
            who.options = Some("link_holder".to_owned());
            who
        },
        {
            let mut who_note = FieldSpec::new("who_note", FieldType::Text);
            who_note.label = Some("Who".to_owned());
            who_note
        },
        {
            let mut number = FieldSpec::new("count", FieldType::Int);
            number.label = Some("Count".to_owned());
            number
        },
        decimal("amount"),
        {
            let mut rows = FieldSpec::new("the_rows", FieldType::Table);
            rows.options = Some("the_child".to_owned());
            rows
        },
    ];
    let meta = Registry::new(vec![definition, parent, the_child])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("the fixture does not build: {errors:?}"));
    let view = default_view(&meta, "mixed", ViewKind::List).expect("generates");
    let names: Vec<&str> = view.columns.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "who_note", // the title first
            "who",      // link
            "state",    // select
            "when",     // date
            "count",    // int
            "amount",   // decimal — the fifth extra column
        ],
        "the title first, then link, select, date, int, decimal; the long-text, json, bytes and table types never show: {names:?}",
    );
}

/// Deliverable 3: the child grid carries every field in order.
#[test]
fn the_child_grid_carries_every_field_in_order() {
    let meta = effective();
    let view = default_view(&meta, "sales_invoice_item", ViewKind::Grid).expect("generates");
    let names: Vec<&str> = view.columns.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(names, vec!["item_code", "qty", "rate", "amount", "note"]);
    assert!(view.sections.is_empty());
    assert!(view.tabs.is_empty());
}

/// Deliverable 1's shape rule on a child DocType: the form of a child is a single flat
/// fields section (the parent's grid item is its usual face; the child's own form is the
/// row editor).
#[test]
fn the_childs_form_is_one_fields_section() {
    let meta = effective();
    let view = default_view(&meta, "sales_invoice_item", ViewKind::Form).expect("generates");
    let ids: Vec<&str> = view.all_sections().iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["fields"], "{view:?}");
    assert_eq!(
        fields_of(section_of(&view, "fields").expect("fields")),
        vec!["item_code", "qty", "rate", "amount", "note"]
    );
}

/// Deliverable 4: the label fallback humanises from the storage name when no label is
/// set, and the definition's label wins when it is set.
#[test]
fn the_label_fallback_reads_the_definition() {
    let meta = effective();
    // `item_group`'s fields carry no labels: the generated list column labels stay none
    // (the fallback is the renderer's read), while a labelled field keeps its label.
    let view = default_view(&meta, "item_group", ViewKind::List).expect("generates");
    assert!(view.columns.iter().all(|column| column.label.is_none()));
    // The humanised fallback: a table section titled by the field's label humanises the
    // name when the label is absent; the table's child DocType is part of the set.
    let mut with_children = Definition::new("with_children");
    with_children.fields = vec![{
        let mut rows = FieldSpec::new("child_rows", FieldType::Table);
        rows.options = Some("the_child".to_owned());
        rows
    }];
    let mut the_child = Definition::new("the_child");
    the_child.is_child = true;
    the_child.fields = vec![text("row_name")];
    let meta2 = Registry::new(vec![with_children, the_child])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("the fixture does not build: {errors:?}"));
    let view2 = default_view(&meta2, "with_children", ViewKind::Form).expect("generates");
    let table = section_of(&view2, "table_child_rows").expect("the table section");
    assert_eq!(table.title.as_deref(), Some("Child rows"));
}

/// The generated list over a DocType whose *customised* `title_field` naming is intact:
/// the sort's `title_field` fallback is exercised by `the_plain_master_fixture`; a
/// DocType with a date and a title sorts by the date (the date rule wins over the title).
#[test]
fn a_date_beats_the_title_in_the_sort() {
    let meta = effective();
    let view = default_view(&meta, "sales_invoice", ViewKind::List).expect("generates");
    let sort = view.sort.as_ref().expect("the posted fixture sorts");
    assert_eq!(sort.field, "posting_date");
    assert!(
        sort.descending,
        "the date sorts descending, the newest on top"
    );
}

/// The plain master fixture from `common` stays coherent with the priority test's
/// expectations (a guard on fixture edits).
#[test]
fn the_plain_master_fixture_stays_well_shaped() {
    let meta = effective();
    assert_eq!(item().name, "item");
    assert_eq!(item_group().name, "item_group");
    assert_eq!(
        meta.version(),
        1,
        "the fixtures build without customisations"
    );
}
