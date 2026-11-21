//! Runs the whole conformance corpus and reports every failing case id at once.
#![allow(missing_docs)]

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Case, Expect, context_of, describe, typed_to_value};
use spot_expr::parse;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

pub fn load() -> Vec<(String, Vec<Case>)> {
    let mut files: Vec<_> = fs::read_dir(corpus_dir())
        .unwrap_or_else(|e| panic!("corpus directory: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text =
                fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let cases: Vec<Case> =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (
                path.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                cases,
            )
        })
        .collect()
}

/// `None` when the case passes, else why not.
fn run(case: &Case) -> Option<String> {
    let parsed = parse(&case.expr);
    match (&case.expect, parsed) {
        (Expect::Error { error, offset }, Err(e)) => {
            if e.code != error {
                Some(format!(
                    "expected {error}, got parse error {} ({})",
                    e.code, e.message
                ))
            } else if offset.is_some_and(|o| o != e.offset) {
                Some(format!(
                    "expected {error} at {:?}, got offset {}",
                    offset, e.offset
                ))
            } else {
                None
            }
        }
        (Expect::Error { error, .. }, Ok(expr)) => {
            let ctx = context_of(case);
            match expr.eval(&ctx) {
                Err(e) if e.code == error => None,
                Err(e) => Some(format!("expected {error}, got {}", e.code)),
                Ok(v) => Some(format!("expected {error}, got {}", describe(&v))),
            }
        }
        (Expect::Ok { ok }, Err(e)) => Some(format!(
            "expected {}, got parse error {} at {} ({})",
            describe(&typed_to_value(ok)),
            e.code,
            e.offset,
            e.message
        )),
        (Expect::Ok { ok }, Ok(expr)) => {
            let want = typed_to_value(ok);
            let ctx = context_of(case);
            match expr.eval(&ctx) {
                Ok(got) if got.same(&want) => None,
                Ok(got) => Some(format!(
                    "expected {}, got {}",
                    describe(&want),
                    describe(&got)
                )),
                Err(e) => Some(format!(
                    "expected {}, got {} ({})",
                    describe(&want),
                    e.code,
                    e.message
                )),
            }
        }
    }
}

#[test]
fn the_corpus_has_at_least_400_cases_and_unique_ids() {
    let files = load();
    let mut ids = std::collections::BTreeSet::new();
    let mut total = 0;
    for (_, cases) in &files {
        for c in cases {
            assert!(ids.insert(c.id.clone()), "duplicate id {}", c.id);
            total += 1;
        }
    }
    assert!(total >= 400, "only {total} cases");
    assert!(files.len() >= 10, "the corpus is grouped by file");
}

#[test]
fn every_case_of_the_corpus_passes() {
    let mut failures = Vec::new();
    let mut total = 0;
    for (file, cases) in load() {
        for case in &cases {
            total += 1;
            if let Some(why) = run(case) {
                failures.push(format!(
                    "{file} {}: `{}`: {why}",
                    case.id,
                    case.expr.replace('\n', "\\n")
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} corpus cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
