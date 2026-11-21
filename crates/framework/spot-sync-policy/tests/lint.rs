//! Every policy lint has a golden failing case, and a consistent policy passes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::{declared, doctype, link, master, required_text, table, text, transactional};
use spot_sync_policy::{
    Class, Direction, FieldName, PolicyProblem, PolicySet, QueuedRecords, ScopeKeyType, SyncPolicy,
    Trigger, codes, lint, lint_change, resolve,
};

fn has(problems: &[PolicyProblem], code: &str) -> bool {
    problems.iter().any(|p| p.code() == code)
}

fn with_excluded(names: &[&str]) -> SyncPolicy {
    let fields: BTreeSet<FieldName> = names.iter().map(|n| FieldName::new(n).unwrap()).collect();
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        fields,
    )
}

#[test]
fn a_consistent_policy_passes() {
    let meta = common::build(vec![
        doctype("item", vec![text("title"), text("cost_price")]),
        doctype("price_list", vec![link("item", "item")]),
    ]);
    let mut set = declared("item", master());
    set.declare("price_list", master());
    let table = resolve(&meta, &set);

    let problems = lint(&table, &meta);
    assert!(
        problems.is_empty(),
        "expected no problems, got {problems:?}"
    );
}

#[test]
fn closed_link_is_refused() {
    let meta = common::build(vec![
        doctype("sales_note", vec![link("scratch", "scratch_note")]),
        doctype("scratch_note", vec![text("body")]),
    ]);
    let set = declared(
        "sales_note",
        transactional(Some(ScopeKeyType::new("warehouse").unwrap())),
    );
    let table = resolve(&meta, &set);

    let problems = lint(&table, &meta);
    assert!(has(&problems, codes::CLOSED_LINK), "got {problems:?}");
}

#[test]
fn a_link_inside_a_child_table_is_refused() {
    let meta = common::build(vec![
        doctype("order", vec![table("lines", "order_line")]),
        common::child("order_line", vec![link("scratch", "scratch_note")]),
        doctype("scratch_note", vec![text("body")]),
    ]);
    let set = declared("order", master());
    let table = resolve(&meta, &set);

    let problems = lint(&table, &meta);
    assert!(has(&problems, codes::CLOSED_LINK), "got {problems:?}");
}

#[test]
fn direction_contradicting_the_class_is_refused() {
    // A master that flows up.
    let meta = common::build(vec![doctype("item", vec![text("title")])]);
    let mut master_up = master();
    master_up.direction = Direction::Up;
    master_up.owner_scope = Some(ScopeKeyType::new("company").unwrap());
    let table = resolve(&meta, &declared("item", master_up));
    let problems = lint(&table, &meta);
    assert!(
        has(&problems, codes::DIRECTION_CONFLICT),
        "got {problems:?}"
    );

    // A local DocType that claims to send.
    let mut local_down = SyncPolicy::local();
    local_down.direction = Direction::Down;
    let table = resolve(&meta, &declared("item", local_down));
    let problems = lint(&table, &meta);
    assert!(
        has(&problems, codes::DIRECTION_CONFLICT),
        "got {problems:?}"
    );

    // A synced class that syncs nothing.
    let mut master_none = master();
    master_none.direction = Direction::None;
    let table = resolve(&meta, &declared("item", master_none));
    let problems = lint(&table, &meta);
    assert!(
        has(&problems, codes::DIRECTION_CONFLICT),
        "got {problems:?}"
    );
}

#[test]
fn an_upward_policy_without_an_owner_scope_is_refused() {
    let meta = common::build(vec![doctype("till_sale", vec![text("total")])]);
    let table = resolve(&meta, &declared("till_sale", transactional(None)));

    let problems = lint(&table, &meta);
    assert!(
        has(&problems, codes::OWNER_SCOPE_MISSING),
        "got {problems:?}"
    );
}

#[test]
fn excluded_fields_that_are_required_link_or_unknown_are_refused() {
    let meta = common::build(vec![
        doctype(
            "item",
            vec![
                required_text("code"),
                link("group", "group"),
                text("cost_price"),
            ],
        ),
        doctype("group", vec![text("title")]),
    ]);
    // `group` is local, but the excluded-link lint fires on the excluded field itself.
    let table = resolve(
        &meta,
        &declared("item", with_excluded(&["code", "group", "ghost"])),
    );

    let problems = lint(&table, &meta);
    let excluded: Vec<&PolicyProblem> = problems
        .iter()
        .filter(|p| p.code() == codes::EXCLUDED_FIELD)
        .collect();
    assert_eq!(
        excluded.len(),
        3,
        "one per excluded field, got {problems:?}"
    );
}

#[test]
fn every_problem_is_returned_at_once() {
    let meta = common::build(vec![
        doctype("item", vec![link("scratch", "scratch_note")]),
        doctype("scratch_note", vec![text("body")]),
    ]);
    let mut policy = transactional(Some(ScopeKeyType::new("warehouse").unwrap()));
    policy.exclude_fields = BTreeSet::from([FieldName::new("ghost").unwrap()]);
    let table = resolve(&meta, &declared("item", policy));

    let problems = lint(&table, &meta);
    assert!(has(&problems, codes::CLOSED_LINK), "got {problems:?}");
    assert!(has(&problems, codes::EXCLUDED_FIELD), "got {problems:?}");
    assert!(problems.len() >= 2);
}

#[test]
fn a_change_that_orphans_queued_records_is_refused() {
    let meta = common::build(vec![doctype("till_sale", vec![text("total")])]);
    let before = resolve(
        &meta,
        &declared(
            "till_sale",
            transactional(Some(ScopeKeyType::new("till").unwrap())),
        ),
    );
    let after = resolve(&meta, &PolicySet::new()); // back to local

    let queued = QueuedRecords::with("till_sale", 7);
    let problems = lint_change(&before, &after, &queued);
    assert!(has(&problems, codes::ORPHAN_QUEUED), "got {problems:?}");
}

#[test]
fn a_change_that_keeps_delivering_queued_records_is_fine() {
    let meta = common::build(vec![doctype("till_sale", vec![text("total")])]);
    let before = resolve(
        &meta,
        &declared(
            "till_sale",
            transactional(Some(ScopeKeyType::new("till").unwrap())),
        ),
    );
    let after = resolve(
        &meta,
        &declared(
            "till_sale",
            transactional(Some(ScopeKeyType::new("till").unwrap())),
        ),
    );

    let queued = QueuedRecords::with("till_sale", 7);
    assert!(lint_change(&before, &after, &queued).is_empty());
}
