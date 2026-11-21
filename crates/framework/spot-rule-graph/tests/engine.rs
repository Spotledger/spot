//! Focused tests: parsing faults, validation, guards, aggregates, determinism, fuel, and the
//! registry's hot swap.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod support;

use spot_expr::DateTime;
use spot_meta::{Definition, FieldSpec, FieldType, Registry as MetaRegistry};
use spot_rule_graph::error::{
    ARITY, FUEL_EXHAUSTED, NOT_A_FIELD, NOT_STRATIFIED, PARSE, UNKNOWN_PREDICATE, UNKNOWN_QUERY,
    UNSAFE,
};
use spot_rule_graph::{Datum, Engine, MemoryFacts, Query, Registry, RuleSet};
use spot_storage_contract::Value;

use support::{decimal, meta, rec};

fn field(name: &str, ty: FieldType, options: Option<&str>) -> FieldSpec {
    let mut spec = FieldSpec::new(name, ty);
    spec.options = options.map(str::to_owned);
    if ty == FieldType::Decimal {
        spec.scale = Some(2);
    }
    spec
}

/// Two accounts (A0, A1), A1 a child of A0, with codes and amounts.
fn accounts_facts() -> MemoryFacts {
    let mut facts = MemoryFacts::new();
    facts.add_field(
        "account",
        "code",
        rec("account", 0),
        Value::Text("a0".to_owned()),
    );
    facts.add_field(
        "account",
        "code",
        rec("account", 1),
        Value::Text("a1".to_owned()),
    );
    facts.add_field("account", "amount", rec("account", 0), decimal("10"));
    facts.add_field("account", "amount", rec("account", 1), decimal("100"));
    facts.add_edge(
        "account.parent_account",
        rec("account", 1),
        rec("account", 0),
    );
    facts
}

#[test]
fn a_parse_fault_reports_its_line_and_column() {
    let error = RuleSet::compile("p(A) :- account.code(A, _)\n", &meta("accounts")).unwrap_err();
    assert_eq!(error.code(), PARSE);
    let problem = &error.problems()[0];
    assert_eq!(problem.line, 2);
    assert_eq!(problem.column, 1);
}

#[test]
fn every_problem_is_reported_at_once() {
    let error =
        RuleSet::compile("p(A) :- nope(A).\nq(B) :- alsonope(B).", &meta("accounts")).unwrap_err();
    assert_eq!(error.problems().len(), 2);
    assert!(error.problems().iter().all(|p| p.code == UNKNOWN_PREDICATE));
    assert_eq!(error.problems()[0].line, 1);
    assert_eq!(error.problems()[1].line, 2);
}

#[test]
fn an_arity_mismatch_is_reported() {
    let error = RuleSet::compile(
        "p(A) :- account.code(A, _).\nq(A) :- p(A, A).",
        &meta("accounts"),
    )
    .unwrap_err();
    assert!(error.problems().iter().any(|p| p.code == ARITY));
}

#[test]
fn an_unsafe_rule_is_rejected() {
    let error = RuleSet::compile("p(A) :- account.code(B, _).", &meta("accounts")).unwrap_err();
    assert!(error.problems().iter().any(|p| p.code == UNSAFE));
}

#[test]
fn a_link_field_is_not_a_field_predicate() {
    let error =
        RuleSet::compile("p(A) :- account_line.account(A, _).", &meta("accounts")).unwrap_err();
    assert!(error.problems().iter().any(|p| p.code == NOT_A_FIELD));
}

#[test]
fn a_cycle_through_negation_is_not_stratifiable() {
    let error = RuleSet::compile(
        "p(A) :- account.code(A, _), not q(A).\nq(A) :- account.code(A, _), not p(A).",
        &meta("accounts"),
    )
    .unwrap_err();
    assert!(error.problems().iter().any(|p| p.code == NOT_STRATIFIED));
}

#[test]
fn a_query_for_an_unknown_predicate_fails() {
    let compiled = RuleSet::compile("p(A) :- account.code(A, _).", &meta("accounts")).unwrap();
    let error = Engine::new()
        .evaluate(&compiled, &accounts_facts(), &Query::new("nope"))
        .unwrap_err();
    assert_eq!(error.code(), UNKNOWN_QUERY);
}

#[test]
fn a_guard_reads_a_bound_variable() {
    let source =
        "big(A) :- account.code(A, _), account.amount(A, Amount), where (doc.Amount > 50).";
    let compiled = RuleSet::compile(source, &meta("accounts")).unwrap();
    let rows = Engine::new()
        .evaluate(&compiled, &accounts_facts(), &Query::new("big"))
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Datum::Record(rec("account", 1))]]);
}

#[test]
fn now_is_the_query_instant() {
    let source = "recent(A) :- account.code(A, _), where (year(now()) >= 2026).";
    let compiled = RuleSet::compile(source, &meta("accounts")).unwrap();
    let at = |text: &str| DateTime::parse(text).unwrap();
    let rows = Engine::new()
        .evaluate(
            &compiled,
            &accounts_facts(),
            &Query::new("recent").at(at("2026-06-01T00:00:00Z")),
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    let rows = Engine::new()
        .evaluate(
            &compiled,
            &accounts_facts(),
            &Query::new("recent").at(at("2025-06-01T00:00:00Z")),
        )
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn count_and_min_aggregate_a_group() {
    let source = "children(A, count<C>) :- account.parent_account(C, A).\n\
                  min_code(A, min<Code>) :- account.parent_account(C, A), account.code(C, Code).";
    let compiled = RuleSet::compile(source, &meta("accounts")).unwrap();
    let engine = Engine::new();
    let facts = accounts_facts();
    let rows = engine
        .evaluate(&compiled, &facts, &Query::new("children"))
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Datum::Record(rec("account", 0)), Datum::Int(1)]]
    );
    let rows = engine
        .evaluate(&compiled, &facts, &Query::new("min_code"))
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![
            Datum::Record(rec("account", 0)),
            Datum::Text("a1".to_owned())
        ]]
    );
}

#[test]
fn a_guard_only_rule_states_a_constant_fact() {
    let compiled = RuleSet::compile("answer(1) :- where (true).", &meta("accounts")).unwrap();
    let rows = Engine::new()
        .evaluate(&compiled, &MemoryFacts::new(), &Query::new("answer"))
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Datum::Int(1)]]);
}

#[test]
fn the_result_does_not_depend_on_fact_order() {
    let source =
        "subtree(A, sum<Amount>) :- account.parent_account(D, A), account.amount(D, Amount).";
    let compiled = RuleSet::compile(source, &meta("accounts")).unwrap();

    let mut forward = MemoryFacts::new();
    forward.add_edge(
        "account.parent_account",
        rec("account", 1),
        rec("account", 0),
    );
    forward.add_field("account", "amount", rec("account", 1), decimal("100"));
    forward.add_field("account", "amount", rec("account", 0), decimal("10"));

    let mut reversed = MemoryFacts::new();
    reversed.add_field("account", "amount", rec("account", 0), decimal("10"));
    reversed.add_field("account", "amount", rec("account", 1), decimal("100"));
    reversed.add_edge(
        "account.parent_account",
        rec("account", 1),
        rec("account", 0),
    );

    let engine = Engine::new();
    let a = engine
        .evaluate(&compiled, &forward, &Query::new("subtree"))
        .unwrap();
    let b = engine
        .evaluate(&compiled, &reversed, &Query::new("subtree"))
        .unwrap();
    assert_eq!(a.rows, b.rows);
}

#[test]
fn fuel_exhaustion_is_reported() {
    let compiled = RuleSet::compile("p(A) :- account.code(A, _).", &meta("accounts")).unwrap();
    let error = Engine::with_fuel(0)
        .evaluate(&compiled, &accounts_facts(), &Query::new("p"))
        .unwrap_err();
    assert_eq!(error.code(), FUEL_EXHAUSTED);
}

#[test]
fn the_registry_swaps_by_content_hash_and_reloads_on_metadata() {
    let accounts = meta("accounts");
    let registry = Registry::new();
    let first = registry
        .register("nodes", "node(A) :- account.code(A, _).", &accounts)
        .unwrap();
    assert_eq!(registry.names(), vec!["nodes".to_owned()]);
    assert_eq!(registry.get("nodes").unwrap().hash(), first.hash());

    // Reloading against the same metadata changes nothing.
    let report = registry.reload(&accounts);
    assert_eq!(report.unchanged, vec!["nodes".to_owned()]);
    assert!(report.reloaded.is_empty());
    assert!(report.failed.is_empty());

    // Turning `code` into a link changes the compiled predicate kind, so the content hash
    // changes and the set is swapped.
    let relinked = {
        let mut account = Definition::new("account");
        account
            .fields
            .push(field("parent_account", FieldType::Link, Some("account")));
        account
            .fields
            .push(field("amount", FieldType::Decimal, None));
        account
            .fields
            .push(field("code", FieldType::Link, Some("account")));
        MetaRegistry::new(vec![account]).unwrap().effective()
    };
    let before = registry.get("nodes").unwrap().hash().clone();
    let report = registry.reload(&relinked);
    assert_eq!(report.reloaded, vec!["nodes".to_owned()]);
    assert_ne!(&before, registry.get("nodes").unwrap().hash());

    // A metadata the set cannot compile against keeps the previous set and is reported.
    let broken = MetaRegistry::new(vec![Definition::new("other")])
        .unwrap()
        .effective();
    let report = registry.reload(&broken);
    assert_eq!(report.failed.len(), 1);
    assert!(registry.get("nodes").is_some());
}

#[test]
fn an_in_flight_evaluation_keeps_the_version_it_started_on() {
    let accounts = meta("accounts");
    let registry = Registry::new();
    let _ = registry
        .register("nodes", "node(A) :- account.code(A, _).", &accounts)
        .unwrap();
    let held = registry.get("nodes").unwrap();
    let _ = registry
        .register("nodes", "node(A) :- account.amount(A, _).", &accounts)
        .unwrap();
    let current = registry.get("nodes").unwrap();
    assert_ne!(held.hash(), current.hash());

    // The handle taken before the swap still evaluates the old program.
    let rows = Engine::new()
        .evaluate(&held, &accounts_facts(), &Query::new("node"))
        .unwrap();
    assert_eq!(rows.len(), 2);
}
