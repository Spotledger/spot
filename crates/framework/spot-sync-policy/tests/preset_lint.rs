//! The catalog-wide preset lints L1 to L7 (WP-SYN-01.T2, ADR-063 section 6): every lint has a
//! seeded case that is rejected and a near miss that is accepted.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{build, child, doctype, link, required_text, table, text};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType};
use spot_sync_policy::preset_lint::codes;
use spot_sync_policy::{
    AdoptMode, ExcludedFields, FieldName, KeyRef, KeyScope, PlainExtras, Preset, PresetPolicy,
    PresetTable, ScopeKeyType, Selector, lint_preset_entity, lint_presets,
};

fn field(name: &str) -> FieldName {
    FieldName::new(name).unwrap()
}

fn key_type(name: &str) -> ScopeKeyType {
    ScopeKeyType::new(name).unwrap()
}

fn typed(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn select(name: &str) -> FieldSpec {
    let mut f = typed(name, FieldType::Select);
    f.options = Some("a\nb\n".to_owned());
    f
}

fn dynamic(name: &str, type_field: &str) -> FieldSpec {
    let mut f = typed(name, FieldType::DynamicLink);
    f.options = Some(type_field.to_owned());
    f
}

fn unique(name: &str) -> FieldSpec {
    let mut f = text(name);
    f.unique = true;
    f
}

fn policy(preset: Preset) -> PresetPolicy {
    PresetPolicy::new(preset)
}

fn scoped(preset: Preset, selector: Selector) -> PresetPolicy {
    let mut p = policy(preset);
    p.scope = Some(selector);
    p
}

fn declared(entries: &[(&str, PresetPolicy)]) -> PresetTable {
    let mut table = PresetTable::new();
    for (name, p) in entries {
        table.set(name, p.clone());
    }
    table
}

fn codes_of(problems: &[spot_sync_policy::PolicyProblem]) -> Vec<(&str, &'static str)> {
    problems.iter().map(|p| (p.doctype(), p.code())).collect()
}

fn run(
    defs: Vec<Definition>,
    entries: &[(&str, PresetPolicy)],
    extras: &PlainExtras,
) -> Vec<spot_sync_policy::PolicyProblem> {
    lint_presets(&declared(entries), &build(defs), extras)
}

// ---- L1 closed links ----

#[test]
fn l1_a_synced_entity_linking_to_a_local_entity_is_rejected() {
    let defs = vec![
        doctype("order", vec![link("customer", "customer")]),
        doctype("customer", vec![text("name")]),
    ];
    let problems = run(
        defs,
        &[("order", policy(Preset::Record))],
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&problems), [("order", codes::LINT_CLOSED_LINK)]);
    assert!(problems[0].detail().contains("customer"));
}

#[test]
fn l1_near_miss_a_synced_target_and_a_local_source_are_accepted() {
    let defs = || {
        vec![
            doctype("order", vec![link("customer", "customer")]),
            doctype("customer", vec![text("name")]),
        ]
    };
    let both = [
        ("order", policy(Preset::Record)),
        ("customer", policy(Preset::Master)),
    ];
    assert!(run(defs(), &both, &PlainExtras::new()).is_empty());
    // A local source may link to anything.
    assert!(run(defs(), &[], &PlainExtras::new()).is_empty());
}

#[test]
fn l1_a_child_row_link_counts_and_the_child_needs_no_policy_of_its_own() {
    let defs = || {
        vec![
            doctype("order", vec![table("lines", "order_line")]),
            child("order_line", vec![link("item", "item")]),
            doctype("item", vec![text("name")]),
        ]
    };
    let problems = run(
        defs(),
        &[("order", policy(Preset::Record))],
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&problems), [("order", codes::LINT_CLOSED_LINK)]);
    assert!(problems[0].detail().contains("lines.item"));
    let ok = run(
        defs(),
        &[
            ("order", policy(Preset::Record)),
            ("item", policy(Preset::Master)),
        ],
        &PlainExtras::new(),
    );
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn l1_a_dynamic_link_checks_every_allowed_target() {
    let defs = || {
        vec![
            doctype("note", vec![text("kind"), dynamic("subject", "kind")]),
            doctype("customer", vec![text("name")]),
            doctype("item", vec![text("name")]),
        ]
    };
    let extras = PlainExtras::new().with_allowed_targets("note", "subject", &["customer", "item"]);
    let entries = [
        ("note", policy(Preset::Record)),
        ("customer", policy(Preset::Master)),
    ];
    // `item` stays local: only that target is a finding.
    let problems = run(defs(), &entries, &extras);
    assert_eq!(codes_of(&problems), [("note", codes::LINT_CLOSED_LINK)]);
    assert!(problems[0].detail().contains("item"));
    // Near miss: every allowed target syncs.
    let all = [
        ("note", policy(Preset::Record)),
        ("customer", policy(Preset::Master)),
        ("item", policy(Preset::Master)),
    ];
    assert!(run(defs(), &all, &extras).is_empty());
}

#[test]
fn l1_a_dynamic_link_without_a_declared_list_is_its_own_finding() {
    let defs = || {
        vec![doctype(
            "note",
            vec![text("kind"), dynamic("subject", "kind")],
        )]
    };
    let problems = run(
        defs(),
        &[("note", policy(Preset::Record))],
        &PlainExtras::new(),
    );
    assert_eq!(
        codes_of(&problems),
        [("note", codes::LINT_DYNAMIC_LINK_UNDECLARED)]
    );
    // An empty list is no list.
    let empty = PlainExtras::new().with_allowed_targets("note", "subject", &[]);
    assert_eq!(
        run(defs(), &[("note", policy(Preset::Record))], &empty).len(),
        1
    );
    // Near miss: on a local entity the field is not a finding.
    assert!(run(defs(), &[], &PlainExtras::new()).is_empty());
}

// ---- L2 direction ----

fn l2_defs() -> Vec<Definition> {
    vec![
        doctype("price_list", vec![link("branch_note", "note")]),
        doctype("note", vec![text("body")]),
    ]
}

#[test]
fn l2_a_master_linking_to_a_record_or_a_handover_is_rejected() {
    for target in [Preset::Record, Preset::Handover] {
        let problems = run(
            l2_defs(),
            &[
                ("price_list", policy(Preset::Master)),
                ("note", scoped(target, Selector::All)),
            ],
            &PlainExtras::new(),
        );
        assert_eq!(
            codes_of(&problems),
            [("price_list", codes::LINT_MASTER_DEPENDS_ON_BRANCH)],
            "{target}"
        );
    }
}

#[test]
fn l2_requires_adopted_allows_the_link_and_other_pairs_are_not_l2() {
    let marked = PlainExtras::new().with_requires_adopted("price_list", "branch_note");
    let entries = [
        ("price_list", policy(Preset::Master)),
        ("note", scoped(Preset::Handover, Selector::All)),
    ];
    assert!(run(l2_defs(), &entries, &marked).is_empty());
    // A master linking to a master is fine; a record linking to a record is not an L2 matter.
    let masters = [
        ("price_list", policy(Preset::Master)),
        ("note", policy(Preset::Master)),
    ];
    assert!(run(l2_defs(), &masters, &PlainExtras::new()).is_empty());
    let records = [
        ("price_list", policy(Preset::Record)),
        ("note", policy(Preset::Record)),
    ];
    assert!(run(l2_defs(), &records, &PlainExtras::new()).is_empty());
}

// ---- L3 optional exclusions ----

fn excluding(preset: Preset, names: &[&str]) -> PresetPolicy {
    let mut p = policy(preset);
    p.excluded_fields = ExcludedFields {
        local: names.iter().map(|n| field(n)).collect(),
        ..ExcludedFields::default()
    };
    p
}

#[test]
fn l3_an_excluded_field_that_is_required_without_a_default_is_rejected() {
    let defs = || vec![doctype("item", vec![required_text("cost"), text("name")])];
    let problems = run(
        defs(),
        &[("item", excluding(Preset::Master, &["cost"]))],
        &PlainExtras::new(),
    );
    assert_eq!(
        codes_of(&problems),
        [("item", codes::LINT_EXCLUDED_REQUIRED)]
    );
    // Near misses: optional, or required with a default, or not excluded at all.
    assert!(
        run(
            defs(),
            &[("item", excluding(Preset::Master, &["name"]))],
            &PlainExtras::new()
        )
        .is_empty()
    );
    let mut with_default = required_text("cost");
    with_default.default = Some("0".to_owned());
    let defaulted = vec![doctype("item", vec![with_default])];
    assert!(
        run(
            defaulted,
            &[("item", excluding(Preset::Master, &["cost"]))],
            &PlainExtras::new()
        )
        .is_empty()
    );
}

#[test]
fn l3_an_excluded_field_the_entity_does_not_have_excludes_nothing() {
    let defs = vec![doctype("item", vec![text("name")])];
    let problems = run(
        defs,
        &[("item", excluding(Preset::Master, &["cst", "cst"]))],
        &PlainExtras::new(),
    );
    assert_eq!(
        codes_of(&problems),
        [("item", codes::LINT_EXCLUDED_UNKNOWN_FIELD)]
    );
}

// ---- L4 scope fields ----

fn keyed(field_name: &str) -> Selector {
    Selector::Key {
        field: field(field_name),
        key_type: key_type("warehouse"),
    }
}

#[test]
fn l4_a_selector_must_read_a_link_enum_or_key_typed_field() {
    let defs = || {
        vec![
            doctype(
                "stock_entry",
                vec![
                    text("label"),
                    link("warehouse", "warehouse"),
                    select("region"),
                    typed("count", FieldType::Int),
                ],
            ),
            doctype("warehouse", vec![text("name")]),
        ]
    };
    let wh = ("warehouse", policy(Preset::Master));
    let bad = run(
        defs(),
        &[
            ("stock_entry", scoped(Preset::Master, keyed("label"))),
            wh.clone(),
        ],
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&bad), [("stock_entry", codes::LINT_SCOPE_FIELD)]);
    // Near misses: a link, an enum, a field declared key-typed.
    for good in ["warehouse", "region"] {
        let p = scoped(Preset::Master, keyed(good));
        assert!(
            run(
                defs(),
                &[("stock_entry", p), wh.clone()],
                &PlainExtras::new()
            )
            .is_empty()
        );
    }
    let extras = PlainExtras::new().with_key_typed("stock_entry", "label");
    let p = scoped(Preset::Master, keyed("label"));
    assert!(run(defs(), &[("stock_entry", p), wh.clone()], &extras).is_empty());
    // A field that does not exist cannot name a key either.
    let missing = run(
        defs(),
        &[("stock_entry", scoped(Preset::Master, keyed("nope"))), wh],
        &PlainExtras::new(),
    );
    assert_eq!(
        codes_of(&missing),
        [("stock_entry", codes::LINT_SCOPE_FIELD)]
    );
}

#[test]
fn l4_owner_key_and_relay_are_checked_and_group_and_all_are_not() {
    let defs = || {
        vec![
            doctype(
                "transfer",
                vec![typed("count", FieldType::Int), link("to_branch", "branch")],
            ),
            doctype("branch", vec![text("name")]),
        ]
    };
    let branch = ("branch", policy(Preset::Master));
    let mut p = policy(Preset::Record);
    p.owner_key = Some(KeyRef {
        field: field("count"),
        key_type: key_type("warehouse"),
    });
    p.relay = Some(keyed("to_branch"));
    let problems = run(
        defs(),
        &[("transfer", p), branch.clone()],
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&problems), [("transfer", codes::LINT_SCOPE_FIELD)]);
    assert!(problems[0].detail().contains("owner_key"));
    let mut fine = policy(Preset::Record);
    fine.relay = Some(Selector::Group("kiosks".to_owned()));
    fine.adopt = Some(AdoptMode::Never);
    assert!(run(defs(), &[("transfer", fine), branch], &PlainExtras::new()).is_empty());
}

// ---- L5 child policy and derived entities ----

#[test]
fn l5_a_child_table_with_its_own_synced_policy_is_rejected() {
    let defs = || {
        vec![
            doctype("order", vec![table("lines", "order_line")]),
            child("order_line", vec![text("note")]),
        ]
    };
    let both = [
        ("order", policy(Preset::Record)),
        ("order_line", policy(Preset::Record)),
    ];
    let problems = run(defs(), &both, &PlainExtras::new());
    assert_eq!(
        codes_of(&problems),
        [("order_line", codes::LINT_CHILD_POLICY)]
    );
    // Near miss: the parent alone; and an explicit `local` on a child is not its own policy.
    let parent = [("order", policy(Preset::Record))];
    assert!(run(defs(), &parent, &PlainExtras::new()).is_empty());
    let local_child = [("order_line", policy(Preset::Local))];
    assert!(run(defs(), &local_child, &PlainExtras::new()).is_empty());
}

#[test]
fn l5_a_derived_entity_that_syncs_is_rejected() {
    let defs = || vec![doctype("stock_balance", vec![text("item")])];
    let extras = PlainExtras::new().with_derived("stock_balance");
    let problems = run(
        defs(),
        &[("stock_balance", policy(Preset::Master))],
        &extras,
    );
    assert_eq!(
        codes_of(&problems),
        [("stock_balance", codes::LINT_DERIVED_SYNCED)]
    );
    // Near misses: derived and local; synced and not derived.
    assert!(run(defs(), &[("stock_balance", policy(Preset::Local))], &extras).is_empty());
    assert!(
        run(
            defs(),
            &[("stock_balance", policy(Preset::Master))],
            &PlainExtras::new()
        )
        .is_empty()
    );
}

// ---- L6 unique keys ----

#[test]
fn l6_a_global_unique_key_on_a_record_or_handover_is_rejected() {
    let defs = || vec![doctype("receipt", vec![unique("number"), text("note")])];
    for preset in [Preset::Record, Preset::Handover] {
        let problems = run(defs(), &[("receipt", policy(preset))], &PlainExtras::new());
        assert_eq!(codes_of(&problems), [("receipt", codes::LINT_UNIQUE_KEY)]);
    }
}

#[test]
fn l6_origin_scoped_series_and_master_keys_are_accepted() {
    let defs = || vec![doctype("receipt", vec![unique("number"), text("note")])];
    for scope in [KeyScope::OriginScoped, KeyScope::PerNodeSeries] {
        let extras = PlainExtras::new().with_key_scope("receipt", "number", scope);
        assert!(run(defs(), &[("receipt", policy(Preset::Record))], &extras).is_empty());
    }
    // The hub is the only writer of a master, so its unique key cannot collide between nodes.
    assert!(
        run(
            defs(),
            &[("receipt", policy(Preset::Master))],
            &PlainExtras::new()
        )
        .is_empty()
    );
}

#[test]
fn l6_a_natural_key_naming_counts_as_a_unique_key() {
    let mut d = doctype("customer", vec![text("code")]);
    d.naming = Some("field:code".to_owned());
    let problems = run(
        vec![d],
        &[("customer", policy(Preset::Handover))],
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&problems), [("customer", codes::LINT_UNIQUE_KEY)]);
}

// ---- L7 reference reach ----

fn pair_defs() -> Vec<Definition> {
    vec![
        doctype(
            "price_list",
            vec![link("tax", "tax_rate"), select("warehouse"), select("a")],
        ),
        doctype(
            "tax_rate",
            vec![text("rate"), select("warehouse"), select("a")],
        ),
    ]
}

fn pair(referrer: Selector, target: Selector) -> Vec<(&'static str, PresetPolicy)> {
    vec![
        ("price_list", scoped(Preset::Master, referrer)),
        ("tax_rate", scoped(Preset::Master, target)),
    ]
}

fn group(tag: &str) -> Selector {
    Selector::Group(tag.to_owned())
}

#[test]
fn l7_a_target_that_does_not_cover_the_referrer_is_a_gap() {
    let problems = run(
        pair_defs(),
        &pair(Selector::All, group("north")),
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&problems), [("price_list", codes::LINT_REACH_GAP)]);
    // A target with no scope reaches nobody.
    let mut none = policy(Preset::Master);
    none.scope = None;
    let entries = [
        ("price_list", scoped(Preset::Master, group("north"))),
        ("tax_rate", none),
    ];
    let problems = run(pair_defs(), &entries, &PlainExtras::new());
    assert_eq!(codes_of(&problems), [("price_list", codes::LINT_REACH_GAP)]);
}

#[test]
fn l7_near_misses_all_the_same_group_and_a_wider_target_are_accepted() {
    for (referrer, target) in [
        (Selector::All, Selector::All),
        (group("north"), Selector::All),
        (group("north"), group("north")),
        (keyed("warehouse"), Selector::All),
    ] {
        let problems = run(
            pair_defs(),
            &pair(referrer.clone(), target.clone()),
            &PlainExtras::new(),
        );
        assert!(
            problems.is_empty(),
            "{referrer:?} -> {target:?}: {problems:?}"
        );
    }
}

#[test]
fn l7_overlapping_groups_and_keys_are_reported_as_undecided() {
    for (referrer, target) in [
        (group("north"), group("south")),
        (keyed("a"), keyed("a")),
        (group("north"), keyed("a")),
    ] {
        let problems = run(
            pair_defs(),
            &pair(referrer.clone(), target.clone()),
            &PlainExtras::new(),
        );
        assert_eq!(
            codes_of(&problems),
            [("price_list", codes::LINT_REACH_UNDECIDED)],
            "{referrer:?} -> {target:?}"
        );
    }
}

#[test]
fn l7_a_carried_link_is_not_compared() {
    let extras = PlainExtras::new().with_carried_by_reference("price_list", "tax");
    let problems = run(pair_defs(), &pair(Selector::All, group("north")), &extras);
    assert!(problems.is_empty(), "{problems:?}");
}

fn chain_defs() -> Vec<Definition> {
    vec![
        doctype("a", vec![link("b", "b")]),
        doctype("b", vec![link("c", "c")]),
        doctype("c", vec![text("x")]),
    ]
}

#[test]
fn l7_a_chain_of_three_masters_with_differing_scopes() {
    let entries = |a: Selector, b: Selector, c: Selector| {
        vec![
            ("a", scoped(Preset::Master, a)),
            ("b", scoped(Preset::Master, b)),
            ("c", scoped(Preset::Master, c)),
        ]
    };
    // Uncarried, every step is compared on its own: a(all) -> b(north) is a gap and b -> c is fine.
    let plain = run(
        chain_defs(),
        &entries(Selector::All, group("north"), group("north")),
        &PlainExtras::new(),
    );
    assert_eq!(codes_of(&plain), [("a", codes::LINT_REACH_GAP)]);
    // Carry a -> b by reference: b is now delivered to every node a reaches, so b's own link to
    // c (scoped to north) leaves those nodes without c. The finding moves to b.
    let carried = PlainExtras::new().with_carried_by_reference("a", "b");
    let moved = run(
        chain_defs(),
        &entries(Selector::All, group("north"), group("north")),
        &carried,
    );
    assert_eq!(codes_of(&moved), [("b", codes::LINT_REACH_GAP)]);
    // Carry both links: nothing is compared.
    let both = carried.clone().with_carried_by_reference("b", "c");
    assert!(
        run(
            chain_defs(),
            &entries(Selector::All, group("north"), group("north")),
            &both
        )
        .is_empty()
    );
    // Near miss: widening c to all covers the whole chain.
    assert!(
        run(
            chain_defs(),
            &entries(Selector::All, group("north"), Selector::All),
            &carried
        )
        .is_empty()
    );
    // And a chain of equal scopes is accepted outright.
    assert!(
        run(
            chain_defs(),
            &entries(group("n"), group("n"), group("n")),
            &PlainExtras::new()
        )
        .is_empty()
    );
}

#[test]
fn l7_link_cycles_terminate() {
    let defs = vec![
        doctype("a", vec![link("b", "b"), table("rows", "a_row")]),
        doctype("b", vec![link("a", "a")]),
        child("a_row", vec![link("b", "b")]),
    ];
    let extras = PlainExtras::new()
        .with_carried_by_reference("a", "b")
        .with_carried_by_reference("b", "a");
    let entries = [
        ("a", scoped(Preset::Master, Selector::All)),
        ("b", scoped(Preset::Master, group("g"))),
    ];
    let problems = run(defs.clone(), &entries, &extras);
    // The carried cycle puts `all` into both audiences; the child row link is not carried.
    assert!(problems.iter().all(|p| p.code() == codes::LINT_REACH_GAP));
    let again = run(defs, &entries, &extras);
    assert_eq!(problems, again);
}

// ---- determinism, entity view, size ----

#[test]
fn output_is_sorted_and_deterministic() {
    let defs = || {
        vec![
            doctype("zeta", vec![link("x", "plain"), unique("n")]),
            doctype("alpha", vec![link("x", "plain"), unique("n")]),
            doctype("plain", vec![text("t")]),
        ]
    };
    let entries = [
        ("zeta", policy(Preset::Record)),
        ("alpha", policy(Preset::Record)),
    ];
    let first = run(defs(), &entries, &PlainExtras::new());
    assert_eq!(first.len(), 4);
    let keys: Vec<_> = first.iter().map(|p| (p.doctype(), p.code())).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert_eq!(first, run(defs(), &entries, &PlainExtras::new()));
}

#[test]
fn the_entity_view_includes_findings_where_the_entity_is_the_target() {
    let meta: EffectiveMeta = build(pair_defs());
    let table = declared(&pair(Selector::All, group("north")));
    let extras = PlainExtras::new();
    let all = lint_presets(&table, &meta, &extras);
    assert_eq!(all.len(), 1);
    assert_eq!(
        lint_preset_entity(&table, &meta, &extras, "price_list"),
        all
    );
    assert_eq!(lint_preset_entity(&table, &meta, &extras, "tax_rate"), all);
    assert!(lint_preset_entity(&table, &meta, &extras, "unrelated").is_empty());
}

#[test]
fn an_empty_table_and_a_policy_for_an_unknown_doctype_find_nothing() {
    let meta = build(pair_defs());
    assert!(lint_presets(&PresetTable::new(), &meta, &PlainExtras::new()).is_empty());
    let table = declared(&[("ghost", policy(Preset::Record))]);
    assert!(lint_presets(&table, &meta, &PlainExtras::new()).is_empty());
}

#[test]
fn a_2000_entity_catalog_lints_well_under_a_second() {
    let mut defs = Vec::new();
    let mut entries = Vec::new();
    for n in 0..2000 {
        let name = format!("e{n:04}");
        let next = format!("e{:04}", (n + 1) % 2000);
        defs.push(doctype(&name, vec![link("next", &next), unique("code")]));
        let preset = match n % 3 {
            0 => Preset::Master,
            1 => Preset::Record,
            _ => Preset::Handover,
        };
        entries.push((name, scoped_if_master(preset, n)));
    }
    let table = {
        let mut t = PresetTable::new();
        for (name, p) in &entries {
            t.set(name, p.clone());
        }
        t
    };
    let meta = build(defs);
    let started = Instant::now();
    let problems = lint_presets(&table, &meta, &PlainExtras::new());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(!problems.is_empty());
}

fn scoped_if_master(preset: Preset, n: usize) -> PresetPolicy {
    if preset == Preset::Master {
        scoped(
            preset,
            if n.is_multiple_of(2) {
                Selector::All
            } else {
                group("g")
            },
        )
    } else {
        policy(preset)
    }
}
