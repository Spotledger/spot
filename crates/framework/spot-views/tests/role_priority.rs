//! Role views: the user's highest-priority role that has a view wins; role views are never
//! merged (ADR-068 §7.2).

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_views::resolve::resolve;
use spot_views::roles::{RoleRef, select_role_view};
use spot_views::viewdef::{Item, Section, ViewDef, ViewKind};

/// A role view for `role` over `fields`.
fn role_view(doctype: &str, role: &str, fields: &[&str]) -> ViewDef {
    ViewDef {
        doctype: doctype.to_owned(),
        kind: ViewKind::Form,
        role: Some(role.to_owned()),
        title: Some(format!("{role} view")),
        density: None,
        tabs: vec![],
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

#[test]
fn the_highest_priority_role_wins() {
    let views = vec![
        role_view("invoice", "clerk", &["customer", "date"]),
        role_view("invoice", "auditor", &["customer", "total"]),
    ];
    let priority = vec!["auditor".to_owned(), "clerk".to_owned()];
    let picked = select_role_view(
        &[RoleRef::new("clerk"), RoleRef::new("auditor")],
        &priority,
        &views,
    );
    let picked = match picked {
        Some(view) => view,
        None => panic!("the auditor has a view"),
    };
    assert_eq!(picked.role.as_deref(), Some("auditor"));

    // Reverse the priority and the clerk's view wins.
    let priority = vec!["clerk".to_owned(), "auditor".to_owned()];
    let picked = select_role_view(
        &[RoleRef::new("clerk"), RoleRef::new("auditor")],
        &priority,
        &views,
    );
    let picked = match picked {
        Some(view) => view,
        None => panic!("the clerk has a view"),
    };
    assert_eq!(picked.role.as_deref(), Some("clerk"));
}

#[test]
fn priority_only_counts_roles_the_user_has() {
    let views = vec![
        role_view("invoice", "clerk", &["customer", "date"]),
        role_view("invoice", "auditor", &["customer", "total"]),
    ];
    let priority = vec!["auditor".to_owned(), "clerk".to_owned()];
    // The user is only a clerk: the auditor's high rank must not apply.
    let picked = select_role_view(&[RoleRef::new("clerk")], &priority, &views);
    let picked = match picked {
        Some(view) => view,
        None => panic!("the clerk has a view"),
    };
    assert_eq!(picked.role.as_deref(), Some("clerk"));

    // No role view for the user's roles: None standard view follows.
    let picked = select_role_view(&[RoleRef::new("guest")], &priority, &views);
    assert!(picked.is_none(), "a guest gets no role view");
}

#[test]
fn an_unlisted_role_ranks_after_listed_ones() {
    let views = vec![
        role_view("invoice", "unlisted", &["customer", "date"]),
        role_view("invoice", "backoffice", &["customer", "total"]),
    ];
    let priority = vec!["backoffice".to_owned()];
    let roles = [RoleRef::new("unlisted"), RoleRef::new("backoffice")];
    let picked = select_role_view(&roles, &priority, &views);
    let picked = match picked {
        Some(view) => view,
        None => panic!("the user holds roles"),
    };
    assert_eq!(
        picked.role.as_deref(),
        Some("backoffice"),
        "listed beats unlisted"
    );
    // The first view of the winning role in list order wins.
    let views = vec![
        role_view("invoice", "backoffice", &["customer", "date", "extra1"]),
        role_view("invoice", "backoffice", &["customer", "date", "extra2"]),
    ];
    let picked = select_role_view(&roles, &priority, &views);
    let picked = match picked {
        Some(view) => view,
        None => panic!("the user holds roles"),
    };
    assert_eq!(picked.title.as_deref(), Some("backoffice view"));
}

#[test]
fn a_role_view_resolves_like_any_base() {
    let meta = meta("invoice", &["customer", "date", "total"]);
    let audit = role_view("invoice", "auditor", &["customer", "total"]);
    let step = spot_views::layer::ViewLayer {
        doctype: "invoice".to_owned(),
        kind: spot_views::layer::LayerKind {
            kind: spot_views::layer::LayerName::Step,
            step: Some("review".to_owned()),
            role: None,
        },
        ops: vec![spot_views::layer::Op::HideField {
            field: "total".to_owned(),
        }],
    };
    let resolved = resolve(&audit, &[step], &meta);
    let fields: Vec<String> = resolved
        .view
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect();
    // `total` hid; the role view never mentioned `date`, so it appends (definition order) —
    // a stored field never silently disappears, even from a role view.
    assert_eq!(fields, vec!["customer".to_owned(), "date".to_owned()]);
    assert_eq!(resolved.hidden, vec!["total"]);
    assert!(meta.definition("invoice").is_some());
}
