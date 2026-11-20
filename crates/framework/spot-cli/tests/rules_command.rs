//! WP-GRA-04.T3: `spot rules check|eval|explain`. The commands run as processes in a scratch
//! directory with no site and no database, except the one case that checks a rule against a
//! site's metadata (PostgreSQL through `SPOT_TEST_PG_URL`, skipped with a printed reason when it
//! is unset). The golden rows are the library's own corpus (`spot-rule-graph/tests/corpus`), run
//! through the command line.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use serde_json::{Value as Json, json};
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Dir(PathBuf);
impl Dir {
    fn new() -> Dir {
        let path = std::env::temp_dir().join(format!("spot_cli_rules_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Dir(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spot(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spot"))
        .args(args)
        .current_dir(dir)
        .env_remove("SPOT_ROOT")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[track_caller]
fn expect_exit(o: &Output, wanted: i32) {
    assert_eq!(
        code(o),
        wanted,
        "stdout: {}\nstderr: {}",
        stdout(o),
        stderr(o)
    );
}

/// An app directory whose DocTypes are the library corpus' metadata fixtures.
fn fixture_app(dir: &Path) -> PathBuf {
    let app = dir.join("fixture");
    std::fs::create_dir_all(app.join("doctypes")).unwrap();
    std::fs::write(
        app.join("app.toml"),
        "[app]\nname = \"rulesfix\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    let field = |name: &str, ty: &str, extra: &str| {
        format!("\n[[fields]]\nname = \"{name}\"\nlabel = \"{name}\"\ntype = \"{ty}\"\n{extra}")
    };
    let doctype = |name: &str, fields: String| {
        format!("name = \"{name}\"\nlabel = \"{name}\"\nmodule = \"rules_fixture\"\n{fields}")
    };
    let files = [
        (
            "account",
            doctype(
                "account",
                [
                    field("code", "text", "max_chars = 40\n"),
                    field("parent_account", "link", "options = \"account\"\n"),
                    field("amount", "decimal", "scale = 2\nprecision = 18\n"),
                ]
                .concat(),
            ),
        ),
        (
            "employee",
            doctype(
                "employee",
                [
                    field("manager", "link", "options = \"employee\"\n"),
                    field("delegate", "link", "options = \"employee\"\n"),
                    field("can_approve", "bool", ""),
                ]
                .concat(),
            ),
        ),
        (
            "item",
            doctype("item", field("component", "link", "options = \"item\"\n")),
        ),
        (
            "bom_change",
            doctype("bom_change", field("item", "link", "options = \"item\"\n")),
        ),
    ];
    for (name, text) in files {
        std::fs::write(app.join(format!("doctypes/{name}.toml")), text).unwrap();
    }
    app
}

// --- the library corpus, through the command line ----------------------------------------------------

struct Case {
    name: String,
    rules: String,
    query: String,
    facts: Json,
    expected: Vec<Vec<String>>,
}

fn record_text(table: &toml::Value) -> String {
    format!(
        "{}#{}",
        table["entity"].as_str().unwrap(),
        table["id"].as_integer().unwrap()
    )
}

fn value_json(value: &toml::Value) -> Json {
    if let Some(record) = value.get("record") {
        json!({ "record": record_text(record) })
    } else if let Some(n) = value.get("int") {
        json!(n.as_integer().unwrap())
    } else if let Some(d) = value.get("decimal") {
        json!({ "decimal": d.as_str().unwrap() })
    } else if let Some(t) = value.get("text") {
        json!(t.as_str().unwrap())
    } else if let Some(b) = value.get("bool") {
        json!(b.as_bool().unwrap())
    } else {
        Json::Null
    }
}

/// The cell the command prints for an expected value; decimals are compared numerically.
fn cell_text(value: &toml::Value) -> String {
    if let Some(record) = value.get("record") {
        record_text(record)
    } else if let Some(n) = value.get("int") {
        n.as_integer().unwrap().to_string()
    } else if let Some(d) = value.get("decimal") {
        format!("decimal:{}", d.as_str().unwrap())
    } else if let Some(t) = value.get("text") {
        serde_json::to_string(t.as_str().unwrap()).unwrap()
    } else if let Some(b) = value.get("bool") {
        b.as_bool().unwrap().to_string()
    } else {
        "null".to_owned()
    }
}

fn corpus() -> Vec<Case> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spot-rule-graph/tests/corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    assert!(files.len() >= 3, "the corpus has its three files");
    files
        .iter()
        .map(|path| {
            let table: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
            let mut facts = serde_json::Map::new();
            for edge in table
                .get("edge")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                facts
                    .entry(edge["type"].as_str().unwrap().to_owned())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap()
                    .push(json!([
                        record_text(&edge["from"]),
                        record_text(&edge["to"])
                    ]));
            }
            for field in table
                .get("field")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let key = format!(
                    "{}.{}",
                    field["doctype"].as_str().unwrap(),
                    field["field"].as_str().unwrap()
                );
                facts
                    .entry(key)
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap()
                    .push(json!([
                        record_text(&field["record"]),
                        value_json(&field["value"])
                    ]));
            }
            let expected = table["expect"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["row"].as_array().unwrap().iter().map(cell_text).collect())
                .collect();
            Case {
                name: table["name"].as_str().unwrap().to_owned(),
                rules: table["rules"].as_str().unwrap().to_owned(),
                query: table["query"].as_str().unwrap().to_owned(),
                facts: Json::Object(facts),
                expected,
            }
        })
        .collect()
}

/// The rows of an `eval` run: the lines before the trailing `# ...` summary, split into cells.
fn rows_of(out: &Output) -> Vec<Vec<String>> {
    stdout(out)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split('\t').map(str::to_owned).collect())
        .collect()
}

fn same_cells(actual: &[Vec<String>], expected: &[Vec<String>]) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(a, e)| {
            a.len() == e.len()
                && a.iter()
                    .zip(e)
                    .all(|(x, y)| match y.strip_prefix("decimal:") {
                        Some(d) => {
                            x.parse::<rust_decimal::Decimal>().ok()
                                == d.parse::<rust_decimal::Decimal>().ok()
                        }
                        None => x == y,
                    })
        })
}

fn write_case(dir: &Path, case: &Case) -> (String, String) {
    let rules = format!("{}.rules", case.name);
    let facts = format!("{}.json", case.name);
    std::fs::write(dir.join(&rules), &case.rules).unwrap();
    std::fs::write(dir.join(&facts), case.facts.to_string()).unwrap();
    (rules, facts)
}

#[test]
fn the_library_corpus_gives_its_golden_rows_through_eval() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    for case in corpus() {
        let (rules, facts) = write_case(dir.path(), &case);
        let out = spot(
            dir.path(),
            &[
                "rules",
                "eval",
                &rules,
                "--facts",
                &facts,
                "--query",
                &case.query,
                "--app",
                &app,
            ],
        );
        expect_exit(&out, 0);
        assert!(
            same_cells(&rows_of(&out), &case.expected),
            "corpus `{}`: {:?} vs {:?}",
            case.name,
            rows_of(&out),
            case.expected
        );
        // The same rule checks clean.
        expect_exit(
            &spot(dir.path(), &["rules", "check", &rules, "--app", &app]),
            0,
        );
    }
}

#[test]
fn evaluation_is_deterministic_over_twenty_runs() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    let case = corpus().into_iter().next().unwrap();
    let (rules, facts) = write_case(dir.path(), &case);
    let args = [
        "rules",
        "eval",
        &rules,
        "--facts",
        &facts,
        "--query",
        &case.query,
        "--app",
        &app,
        "--json",
    ];
    let first = stdout(&spot(dir.path(), &args));
    for _ in 0..19 {
        assert_eq!(stdout(&spot(dir.path(), &args)), first);
    }
}

#[test]
fn a_query_with_a_constant_keeps_only_the_matching_rows() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    let case = corpus().into_iter().find(|c| c.name == "approver").unwrap();
    let (rules, facts) = write_case(dir.path(), &case);
    let out = spot(
        dir.path(),
        &[
            "rules",
            "eval",
            &rules,
            "--facts",
            &facts,
            "--query",
            "approver(employee#1, _)",
            "--app",
            &app,
        ],
    );
    expect_exit(&out, 0);
    assert_eq!(rows_of(&out), vec![vec!["employee#1", "employee#0"]]);
    // The wrong arity is a usage error.
    let out = spot(
        dir.path(),
        &[
            "rules",
            "eval",
            &rules,
            "--facts",
            &facts,
            "--query",
            "approver(_)",
            "--app",
            &app,
        ],
    );
    expect_exit(&out, 1);
}

// --- check ----------------------------------------------------------------------------------------------

#[test]
fn check_reports_every_problem_at_its_line_and_column_sorted() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    // Four problems on four lines: an unknown predicate, an arity mismatch, an unsafe head
    // variable and a guard over an unbound variable.
    let text = "\
a(X) :- nothing.here(X, _).
b(X) :- employee.manager(X).
c(X, Y) :- employee.manager(X, _).
d(X) :- employee.manager(X, _), where (doc.Z > 1).
";
    std::fs::write(dir.path().join("bad.rules"), text).unwrap();
    let out = spot(dir.path(), &["rules", "check", "bad.rules", "--app", &app]);
    expect_exit(&out, 2);
    let report = stderr(&out);
    let positions: Vec<(u32, u32)> = report
        .lines()
        .filter(|l| l.starts_with("bad.rules:"))
        .map(|l| {
            let mut parts = l.splitn(4, ':');
            parts.next();
            (
                parts.next().unwrap().parse().unwrap(),
                parts.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let lines: std::collections::BTreeSet<u32> = positions.iter().map(|(l, _)| *l).collect();
    assert_eq!(
        lines,
        [1, 2, 3, 4].into_iter().collect(),
        "every line's problem is reported: {report}"
    );
    assert!(
        positions.windows(2).all(|w| w[0] <= w[1]),
        "sorted: {positions:?}"
    );
    assert!(report.contains(" problems"), "{report}");
    // JSON is the same facts.
    let json = spot(
        dir.path(),
        &["rules", "check", "bad.rules", "--app", &app, "--json"],
    );
    expect_exit(&json, 2);
    let value: Json = serde_json::from_str(stdout(&json).trim()).unwrap();
    assert_eq!(value["problems"].as_array().unwrap().len(), positions.len());
}

#[test]
fn an_unsafe_rule_and_a_non_stratifiable_program_are_refused_with_their_codes() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    std::fs::write(
        dir.path().join("unsafe.rules"),
        "bad(X, Y) :- employee.manager(X, _).\n",
    )
    .unwrap();
    let out = spot(
        dir.path(),
        &["rules", "check", "unsafe.rules", "--app", &app],
    );
    expect_exit(&out, 2);
    assert!(
        stderr(&out).contains("rule_graph.unsafe"),
        "{}",
        stderr(&out)
    );

    std::fs::write(
        dir.path().join("cycle.rules"),
        "p(X) :- employee.manager(X, _), not q(X).\nq(X) :- employee.manager(X, _), not p(X).\n",
    )
    .unwrap();
    let out = spot(
        dir.path(),
        &["rules", "check", "cycle.rules", "--app", &app],
    );
    expect_exit(&out, 2);
    assert!(
        stderr(&out).contains("rule_graph.not_stratified"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_report_shows_one_source_line_at_most_two_hundred_characters_and_no_more() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    let long = format!(
        "a(X) :- nothing.here(X, _), {}.\n",
        "employee.manager(X, _), ".repeat(40)
    );
    std::fs::write(dir.path().join("long.rules"), &long).unwrap();
    let out = spot(dir.path(), &["rules", "check", "long.rules", "--app", &app]);
    expect_exit(&out, 2);
    let shown = stderr(&out)
        .lines()
        .find(|l| l.starts_with("    a(X)"))
        .map(|l| l.trim_start().chars().count())
        .unwrap();
    assert!(shown <= 200, "the source line is cut: {shown}");
}

#[test]
fn an_unreadable_rule_file_is_exit_three_and_usage_is_one_or_clap_two() {
    let dir = Dir::new();
    expect_exit(&spot(dir.path(), &["rules", "check", "missing.rules"]), 3);
    expect_exit(&spot(dir.path(), &["rules", "check"]), 2);
    expect_exit(
        &spot(
            dir.path(),
            &["rules", "check", "x", "--site", "a", "--app", "b"],
        ),
        2,
    );
}

// --- eval limits ----------------------------------------------------------------------------------------

#[test]
fn a_runaway_rule_exhausts_its_fuel_with_no_rows() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    // A chain of 400 accounts: the closure derives about 80,000 pairs; one fuel unit per derived
    // tuple makes a limit of 100 break long before.
    let edges: Vec<Json> = (1..400)
        .map(|i| json!([format!("account#{i}"), format!("account#{}", i - 1)]))
        .collect();
    std::fs::write(
        dir.path().join("facts.json"),
        json!({ "account.parent_account": edges }).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("closure.rules"),
        "up(A, B) :- account.parent_account(A, B).\nup(A, C) :- account.parent_account(A, B), up(B, C).\n",
    )
    .unwrap();
    let out = spot(
        dir.path(),
        &[
            "rules",
            "eval",
            "closure.rules",
            "--facts",
            "facts.json",
            "--query",
            "up",
            "--app",
            &app,
            "--fuel",
            "100",
        ],
    );
    expect_exit(&out, 2);
    assert!(stderr(&out).contains("fuel_exhausted"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "no partial answer");
}

#[test]
fn a_fact_file_over_the_limit_is_refused_without_being_read_whole() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    std::fs::write(
        dir.path().join("r.rules"),
        "up(A, B) :- account.parent_account(A, B).\n",
    )
    .unwrap();
    // A sparse file of 51 MiB: nothing is allocated for it, and reading it whole would be slow.
    let big = std::fs::File::create(dir.path().join("big.json")).unwrap();
    big.set_len(51 * 1024 * 1024).unwrap();
    let out = spot(
        dir.path(),
        &[
            "rules", "eval", "r.rules", "--facts", "big.json", "--query", "up", "--app", &app,
        ],
    );
    expect_exit(&out, 2);
    assert!(stderr(&out).contains("not read"), "{}", stderr(&out));
}

#[test]
fn facts_that_are_not_valid_are_refused_without_echoing_their_values() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    std::fs::write(
        dir.path().join("r.rules"),
        "up(A, B) :- account.parent_account(A, B).\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("f.json"),
        r#"{"account.parent_account": [["account#1", "SECRETVALUE"]]}"#,
    )
    .unwrap();
    let out = spot(
        dir.path(),
        &[
            "rules", "eval", "r.rules", "--facts", "f.json", "--query", "up", "--app", &app,
        ],
    );
    expect_exit(&out, 2);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("tuple 0"), "{text}");
    assert!(
        !text.contains("SECRETVALUE"),
        "fact values are not echoed: {text}"
    );
}

#[cfg(unix)]
#[test]
fn a_facts_symlink_to_outside_the_working_tree_is_refused() {
    let dir = Dir::new();
    let outside = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    std::fs::write(
        dir.path().join("r.rules"),
        "up(A, B) :- account.parent_account(A, B).\n",
    )
    .unwrap();
    std::fs::write(outside.path().join("f.json"), "{}").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("f.json"),
        dir.path().join("linked.json"),
    )
    .unwrap();
    let out = spot(
        dir.path(),
        &[
            "rules",
            "eval",
            "r.rules",
            "--facts",
            "linked.json",
            "--query",
            "up",
            "--app",
            &app,
        ],
    );
    expect_exit(&out, 2);
    assert!(
        stderr(&out).contains("outside the working tree"),
        "{}",
        stderr(&out)
    );
}

// --- explain --------------------------------------------------------------------------------------------

#[test]
fn explain_prints_the_strata_of_a_program_with_negation() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    let case = corpus().into_iter().find(|c| c.name == "approver").unwrap();
    let (rules, _) = write_case(dir.path(), &case);
    let out = spot(dir.path(), &["rules", "explain", &rules, "--app", &app]);
    expect_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("stratum 0"), "{text}");
    assert!(
        text.contains("stratum 1"),
        "negation puts approver above has_delegate: {text}"
    );
    assert!(text.contains("has_delegate/1"), "{text}");
    assert!(text.contains("negated"), "{text}");
    assert!(text.contains("employee.can_approve/2 (field"), "{text}");
    // Deterministic.
    assert_eq!(
        stdout(&spot(
            dir.path(),
            &["rules", "explain", &rules, "--app", &app]
        )),
        text
    );
    // An aggregate is marked.
    let rollup = corpus()
        .into_iter()
        .find(|c| c.name == "account_rollup")
        .unwrap();
    let (rules, _) = write_case(dir.path(), &rollup);
    let out = spot(dir.path(), &["rules", "explain", &rules, "--app", &app]);
    expect_exit(&out, 0);
    assert!(stdout(&out).contains("aggregate"), "{}", stdout(&out));
}

#[test]
fn explain_warns_about_rules_nothing_query_able_reaches() {
    let dir = Dir::new();
    let app = fixture_app(dir.path());
    let app = app.to_str().unwrap().to_owned();
    // `p` and `q` read only each other: neither is an output, and no output reads them.
    std::fs::write(
        dir.path().join("dead.rules"),
        "p(X) :- employee.manager(X, _), q(X).\nq(X) :- employee.manager(X, _), p(X).\nok(X) :- employee.manager(X, _).\n",
    )
    .unwrap();
    let out = spot(
        dir.path(),
        &["rules", "explain", "dead.rules", "--app", &app],
    );
    expect_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("warning:"), "{text}");
    assert!(text.contains("`p`") && text.contains("`q`"), "{text}");
    assert!(!text.contains("rule for `ok`"), "{text}");
}

#[test]
fn help_of_spot_lists_rules() {
    let dir = Dir::new();
    let out = spot(dir.path(), &["--help"]);
    expect_exit(&out, 0);
    assert!(
        stdout(&out)
            .lines()
            .any(|l| l.trim_start().starts_with("rules")),
        "rules is listed"
    );
    let out = spot(dir.path(), &["rules", "--help"]);
    expect_exit(&out, 0);
    for name in ["check", "eval", "explain"] {
        assert!(stdout(&out).contains(name), "{name}");
    }
}

// --- a site (PostgreSQL) --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_against_a_site_accepts_a_real_edge_type_and_refuses_an_unknown_predicate() {
    let Some(env) = common::Env::new("").await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let _first_secret = env.site();
    let work = env.root.0.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("ok.rules"), "pt(T, P) :- task.project(T, P).\n").unwrap();
    std::fs::write(work.join("bad.rules"), "pt(T, P) :- task.nonesuch(T, P).\n").unwrap();
    let root = env.root.0.to_str().unwrap().to_owned();
    let run = |file: &str| {
        Command::new(env!("CARGO_BIN_EXE_spot"))
            .args(["--root", &root, "rules", "check", file, "--site", &env.site])
            .current_dir(&work)
            .output()
            .unwrap()
    };
    expect_exit(&run("ok.rules"), 0);
    let bad = run("bad.rules");
    expect_exit(&bad, 2);
    assert!(
        stderr(&bad).contains("rule_graph.unknown_predicate"),
        "{}",
        stderr(&bad)
    );
}
