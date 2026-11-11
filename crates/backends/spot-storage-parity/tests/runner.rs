//! The runner itself: a deliberately wrong backend is caught and the report names the rule,
//! the divergence register behaves in all three states, and the completeness check bites.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_storage_contract::{
    Catalog, Change, Document, DocumentData, FeedPosition, Name, Query, RecordId, Rows,
    StorageResult, Value, Version,
};
use spot_storage_fake::FakeBackend;
use spot_storage_parity::cases::{parse_cases, parse_register};
use spot_storage_parity::rules::parse_rules;
use spot_storage_parity::{
    BackendFactory, BackendRun, CaseNeeds, Driver, FakeFactory, Observed, ParityRunner,
    StorageDriver,
};

/// A driver that lowercases every text on the way in: the bug the matrix must catch.
struct Lowercasing<D>(D);

fn lowered(doc: &DocumentData) -> DocumentData {
    let mut d = doc.clone();
    let lower = |v: &mut Value| {
        if let Value::Text(t) = v {
            *t = t.to_lowercase();
        }
    };
    d.fields.values_mut().for_each(lower);
    for rows in d.children.values_mut() {
        for r in rows {
            r.fields.values_mut().for_each(lower);
        }
    }
    d
}

impl<D: Driver> Driver for Lowercasing<D> {
    async fn insert(&self, docs: &[DocumentData], max_writes: Option<u32>) -> StorageResult<()> {
        let docs: Vec<DocumentData> = docs.iter().map(lowered).collect();
        self.0.insert(&docs, max_writes).await
    }
    async fn insert_then_delete(
        &self,
        docs: &[DocumentData],
        max_writes: Option<u32>,
    ) -> StorageResult<()> {
        let docs: Vec<DocumentData> = docs.iter().map(lowered).collect();
        self.0.insert_then_delete(&docs, max_writes).await
    }
    async fn changes(&self, after: FeedPosition, limit: u32) -> StorageResult<Vec<Change>> {
        self.0.changes(after, limit).await
    }
    async fn update(
        &self,
        doc: &DocumentData,
        expected: Version,
        max_writes: Option<u32>,
    ) -> StorageResult<Version> {
        self.0.update(&lowered(doc), expected, max_writes).await
    }
    async fn delete(
        &self,
        entity: &Name,
        id: RecordId,
        expected: Version,
        max_writes: Option<u32>,
    ) -> StorageResult<()> {
        self.0.delete(entity, id, expected, max_writes).await
    }
    async fn load(&self, entity: &Name, id: RecordId) -> StorageResult<Option<Document>> {
        self.0.load(entity, id).await
    }
    async fn query(&self, query: &Query) -> StorageResult<Rows> {
        self.0.query(query).await
    }
    async fn read(&self, query: &Query) -> StorageResult<Rows> {
        self.0.read(query).await
    }
}

struct WrongFactory;

impl BackendFactory for WrongFactory {
    type Driver = Lowercasing<StorageDriver<FakeBackend>>;

    fn name(&self) -> &'static str {
        "wrong"
    }

    async fn fresh(
        &self,
        catalog: Arc<Catalog>,
        needs: &CaseNeeds<'_>,
    ) -> StorageResult<Option<Self::Driver>> {
        Ok(FakeFactory.fresh(catalog, needs).await?.map(Lowercasing))
    }
}

#[tokio::test]
async fn a_backend_that_lowercases_text_is_caught_and_the_rule_is_named() {
    let runner = ParityRunner::from_repo().unwrap();
    let right = runner.run_backend(&FakeFactory).await;
    let wrong = runner.run_backend(&WrongFactory).await;
    let report = runner.report(&[right, wrong], "fake");
    assert!(!report.is_green(), "a lowercasing backend must not pass");
    // Only the wrong backend is blamed, and the rules it breaks are named.
    let broken: Vec<&str> = report
        .unregistered
        .iter()
        .map(|d| d.rule.as_str())
        .collect();
    for rule in ["SEM-TXT-01", "SEM-TXT-10", "SEM-RND-01"] {
        assert!(
            broken.contains(&rule),
            "{rule} should be named; got {broken:?}"
        );
    }
    assert!(
        report
            .unregistered
            .iter()
            .all(|d| d.failed_on.iter().all(|b| b == "wrong"))
    );
    let text = report.to_string();
    assert!(
        text.contains("SEM-TXT-01") && text.contains("DIFFERS"),
        "{text}"
    );
    // The counts per backend show the damage on the wrong backend only.
    let txt01 = &report.rules["SEM-TXT-01"].per_backend;
    assert_eq!(txt01["fake"].failed, 0);
    assert!(txt01["wrong"].failed > 0);
}

const RULES: &str = "\
| SEM-AAA-01 | decided | a rule | src |
| SEM-AAA-02 | decided | another | src |
| SEM-AAA-03 | open | a question? | none |
";

const CASES: &str = r#"
[[case]]
id = "c1"
rule = "SEM-AAA-01"
title = "t"
setup = ['a code="t:x"']
op = "load"
doc = "a"
fields = ["code"]
expect = { rows = [["t:x"]] }

[[case]]
id = "c2"
rule = "SEM-AAA-03"
title = "open case"
op = "write"
write = ['insert a code="t:x"']
expect = { same_as_reference = true }
"#;

fn run(backend: &str, c1: Observed, c2: Observed) -> BackendRun {
    BackendRun {
        backend: backend.to_owned(),
        outcomes: BTreeMap::from([("c1".to_owned(), c1), ("c2".to_owned(), c2)]),
        by_harness: BTreeSet::new(),
    }
}

fn rows_x() -> Observed {
    Observed::Rows(vec![vec!["t:\"x\"".to_owned()]])
}

fn runner_with(register: &str) -> ParityRunner {
    ParityRunner::new(
        parse_cases(CASES).unwrap(),
        parse_register(register).unwrap(),
        parse_rules(RULES).unwrap(),
    )
}

/// ADR-087 option B: a construct the fake does not model is proven on PostgreSQL, and the fake's
/// answer for it is proven by a case scoped to the fake.
const SCOPED_CASES: &str = r#"
[[case]]
id = "pg-only"
rule = "SEM-AAA-01"
title = "a construct the fake does not model"
scope = "postgres"
catalog = "geo"
op = "prepare"
expect = { ok = true }

[[case]]
id = "fake-only"
rule = "SEM-AAA-01"
title = "the fake's answer for that construct, from its capability"
scope = "fake"
catalog = "range"
op = "prepare"
expect = { error = "unsupported" }
"#;

/// A case runs only on the backends its scope names: the fake never runs the PostgreSQL-only
/// case (it is counted as a skip, never as a pass), and it does run the fake-only one, where the
/// capability answer is the verdict.
#[tokio::test]
async fn a_case_runs_only_on_the_backends_its_scope_names() {
    let runner = ParityRunner::new(
        parse_cases(SCOPED_CASES).unwrap(),
        Vec::new(),
        parse_rules(RULES).unwrap(),
    );
    let fake = runner.run_backend(&FakeFactory).await;
    assert_eq!(fake.outcomes["pg-only"], Observed::Skipped);
    assert_eq!(
        fake.outcomes["fake-only"],
        Observed::Error("unsupported".to_owned())
    );
    let report = runner.report(&[fake], "fake");
    let tally = &report.rules["SEM-AAA-01"].per_backend["fake"];
    assert_eq!((tally.passed, tally.failed, tally.skipped), (1, 0, 1));
    assert!(report.is_green(), "{report}");
}

#[test]
fn an_unlisted_difference_fails_and_names_the_rule() {
    let runner = runner_with("");
    let runs = [
        run("postgres", rows_x(), Observed::Ok),
        run(
            "fake",
            Observed::Rows(vec![vec!["t:\"X\"".to_owned()]]),
            Observed::Ok,
        ),
    ];
    let report = runner.report(&runs, "postgres");
    assert!(!report.is_green());
    assert_eq!(report.unregistered.len(), 1);
    assert_eq!(report.unregistered[0].rule, "SEM-AAA-01");
    assert_eq!(report.unregistered[0].failed_on, ["fake"]);
    assert!(report.to_string().contains("SEM-AAA-01"));
    assert_eq!(report.open_rules_with_cases, ["SEM-AAA-03"]);
}

#[test]
fn a_listed_difference_is_reported_but_does_not_fail() {
    let register = r#"
[[divergence]]
rule = "SEM-AAA-01"
reason = "the fake upper-cases for now"
owner = "lead"
"#;
    let runner = runner_with(register);
    let runs = [
        run("postgres", rows_x(), Observed::Ok),
        run(
            "fake",
            Observed::Rows(vec![vec!["t:\"X\"".to_owned()]]),
            Observed::Ok,
        ),
    ];
    let report = runner.report(&runs, "postgres");
    assert!(report.is_green(), "{report}");
    assert_eq!(report.registered.len(), 1);
    assert!(report.to_string().contains("known divergences"));
}

#[test]
fn a_listed_divergence_that_no_longer_diverges_fails() {
    let register = r#"
[[divergence]]
rule = "SEM-AAA-01"
reason = "the fake upper-cases for now"
owner = "lead"
"#;
    let runner = runner_with(register);
    let runs = [
        run("postgres", rows_x(), Observed::Ok),
        run("fake", rows_x(), Observed::Ok),
    ];
    let report = runner.report(&runs, "postgres");
    assert!(!report.is_green());
    assert_eq!(report.stale_register.len(), 1);
    assert!(report.to_string().contains("STALE"));
}

#[test]
fn a_register_entry_can_name_one_case_only() {
    let register = r#"
[[divergence]]
rule = "SEM-AAA-01"
case = "some-other-case"
reason = "x"
owner = "lead"
"#;
    let runner = runner_with(register);
    let runs = [
        run("postgres", rows_x(), Observed::Ok),
        run("fake", Observed::Rows(vec![]), Observed::Ok),
    ];
    let report = runner.report(&runs, "postgres");
    // The failing case is c1, not the listed one: unlisted, and the entry is stale.
    assert_eq!(report.unregistered.len(), 1);
    assert_eq!(report.stale_register.len(), 1);
}

#[test]
fn same_as_reference_compares_with_the_reference_backend() {
    let runner = runner_with("");
    let runs = [
        run(
            "postgres",
            rows_x(),
            Observed::Error("invalid_value".into()),
        ),
        run("fake", rows_x(), Observed::Ok),
    ];
    let report = runner.report(&runs, "postgres");
    assert_eq!(report.unregistered.len(), 1);
    assert_eq!(report.unregistered[0].case, "c2");
    assert_eq!(report.unregistered[0].rule, "SEM-AAA-03");
}

#[test]
fn a_skipped_backend_is_reported_and_judges_nothing() {
    let runner = runner_with("");
    let runs = [
        run("postgres", Observed::Skipped, Observed::Skipped),
        run("fake", rows_x(), Observed::Ok),
    ];
    let report = runner.report(&runs, "postgres");
    assert!(report.is_green(), "{report}");
    assert_eq!(report.skipped_backends, ["postgres"]);
    assert_eq!(report.backends, ["fake"]);
}

#[test]
fn a_broken_case_fails_on_every_backend() {
    let runner = runner_with("");
    let runs = [
        run("postgres", Observed::BrokenCase("bad".into()), Observed::Ok),
        run("fake", Observed::BrokenCase("bad".into()), Observed::Ok),
    ];
    let report = runner.report(&runs, "postgres");
    assert!(!report.is_green());
    assert_eq!(report.unregistered[0].failed_on.len(), 2);
}

#[test]
fn completeness_finds_a_decided_rule_without_a_case() {
    let runner = runner_with("");
    let problems = runner.completeness_problems();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("SEM-AAA-02") && p.contains("no case")),
        "{problems:?}"
    );
    // The open rule needs no case; the unused value kinds are reported too.
    assert!(!problems.iter().any(|p| p.contains("SEM-AAA-03")));
    assert!(problems.iter().any(|p| p.contains("Bytes")));
    assert!(problems.iter().any(|p| p.contains("Suffix")));
}

#[test]
fn completeness_finds_a_case_naming_a_missing_rule() {
    let cases = CASES.replace("SEM-AAA-03", "SEM-ZZZ-99");
    let runner = ParityRunner::new(
        parse_cases(&cases).unwrap(),
        vec![],
        parse_rules(RULES).unwrap(),
    );
    let problems = runner.completeness_problems();
    assert!(
        problems.iter().any(|p| p.contains("SEM-ZZZ-99")),
        "{problems:?}"
    );
    let runs = [run("fake", rows_x(), Observed::Ok)];
    let report = runner.report(&runs, "fake");
    assert!(!report.is_green());
    assert!(report.to_string().contains("SEM-ZZZ-99"));
}

#[test]
fn a_case_needs_exactly_one_expectation() {
    let both = CASES.replace(
        "expect = { rows = [[\"t:x\"]] }",
        "expect = { rows = [[\"t:x\"]], ok = true }",
    );
    let cases = parse_cases(&both).unwrap();
    assert!(spot_storage_parity::cases::check_shape(&cases[0]).is_err());
    assert!(parse_cases("[[case]]\nid = 'x'\nbogus = 1").is_err());
}
