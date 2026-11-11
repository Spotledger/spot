//! The rounding corpus: every case in `tests/corpus/*.json`, run through the crate.
//!
//! The cases are data (operation, inputs as decimal strings, rule, expected decimal string or
//! expected error code). `generate.py` writes all of them except `handwritten.json` from exact
//! rational arithmetic and the documented modes; `handwritten.json` was worked out by hand.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use rust_decimal::Decimal;
use serde_json::Value;
use spot_money::{
    Currency, Money, MoneyError, Quantity, RoundingRule, RoundingScope, TaxLines, allocate,
    exclusive_lines, inclusive_lines, mul, mul_exact, parse_decimal, percent, sum_decimals,
    tax_exclusive_to_inclusive, tax_in_inclusive, tax_inclusive_to_exclusive, tax_on_exclusive,
};

fn dec(v: &Value) -> Decimal {
    parse_decimal(v.as_str().unwrap()).unwrap()
}

fn decs(v: &Value) -> Vec<Decimal> {
    v.as_array().unwrap().iter().map(dec).collect()
}

fn money(case: &Value, field: &str) -> Money {
    let currency = Currency::new(case["currency"].as_str().unwrap()).unwrap();
    Money::new(dec(&case[field]), currency)
}

/// The rule, or the error that building it gave (its code is in the deserializer's message).
fn rule(case: &Value) -> Result<RoundingRule, String> {
    serde_json::from_value(case["rule"].clone()).map_err(|e| e.to_string())
}

fn scope(case: &Value) -> RoundingScope {
    match case["scope"].as_str().unwrap() {
        "per_line" => RoundingScope::PerLine,
        "on_total" => RoundingScope::OnTotal,
        other => panic!("unknown scope {other}"),
    }
}

/// What a case produced: a text, a list of texts, a tax result, or an error code.
enum Outcome {
    Text(String),
    Numeric(Decimal),
    List(Vec<String>),
    Tax(TaxLines),
    Error(String),
}

fn code(e: &MoneyError) -> Outcome {
    Outcome::Error(e.code().to_owned())
}

fn run(case: &Value) -> Outcome {
    let op = case["op"].as_str().unwrap();
    if op == "add_currencies" {
        let (l, r) = (
            Currency::new(case["left"].as_str().unwrap()).unwrap(),
            Currency::new(case["right"].as_str().unwrap()).unwrap(),
        );
        return match Money::new(Decimal::ONE, l).checked_add(&Money::new(Decimal::ONE, r)) {
            Ok(m) => Outcome::Text(m.to_string()),
            Err(e) => code(&e),
        };
    }
    // Operations without a rule.
    match op {
        "mul_exact" => {
            let q = Quantity::new(dec(&case["qty"]), None);
            return match mul_exact(&money(case, "price"), &q) {
                Ok(m) => Outcome::Numeric(m.value()),
                Err(e) => code(&e),
            };
        }
        "percent" => {
            return match percent(&money(case, "amount"), dec(&case["rate"])) {
                Ok(m) => Outcome::Numeric(m.value()),
                Err(e) => code(&e),
            };
        }
        "sum" => {
            return match sum_decimals(decs(&case["values"])) {
                Ok(d) => Outcome::Numeric(d),
                Err(e) => code(&e),
            };
        }
        _ => {}
    }
    let rule = match rule(case) {
        Ok(r) => r,
        Err(message) => {
            // `money.bad_scale: ...` and friends.
            let code = message.split(':').next().unwrap_or("").to_owned();
            return Outcome::Error(code);
        }
    };
    let text = |r: Result<Decimal, MoneyError>| match r {
        Ok(d) => Outcome::Text(d.to_string()),
        Err(e) => code(&e),
    };
    match op {
        "round" => text(rule.apply(dec(&case["value"]))),
        "mul" => {
            let q = Quantity::new(dec(&case["qty"]), None);
            match mul(&money(case, "price"), &q, &rule) {
                Ok(m) => Outcome::Text(m.value().to_string()),
                Err(e) => code(&e),
            }
        }
        "allocate" => match allocate(&money(case, "total"), &decs(&case["weights"]), &rule) {
            Ok(parts) => Outcome::List(parts.iter().map(|p| p.value().to_string()).collect()),
            Err(e) => code(&e),
        },
        "tax_incl_to_excl" => text(tax_inclusive_to_exclusive(
            dec(&case["gross"]),
            dec(&case["rate"]),
            &rule,
        )),
        "tax_excl_to_incl" => text(tax_exclusive_to_inclusive(
            dec(&case["net"]),
            dec(&case["rate"]),
            &rule,
        )),
        "tax_on_exclusive" => text(tax_on_exclusive(
            dec(&case["net"]),
            dec(&case["rate"]),
            &rule,
        )),
        "tax_in_inclusive" => text(tax_in_inclusive(
            dec(&case["gross"]),
            dec(&case["rate"]),
            &rule,
        )),
        "tax_lines_exclusive" => match exclusive_lines(
            &decs(&case["amounts"]),
            dec(&case["rate"]),
            &rule,
            scope(case),
        ) {
            Ok(t) => Outcome::Tax(t),
            Err(e) => code(&e),
        },
        "tax_lines_inclusive" => match inclusive_lines(
            &decs(&case["amounts"]),
            dec(&case["rate"]),
            &rule,
            scope(case),
        ) {
            Ok(t) => Outcome::Tax(t),
            Err(e) => code(&e),
        },
        other => panic!("unknown op {other}"),
    }
}

fn check(file: &str, index: usize, case: &Value) {
    let label = format!("{file} #{index} {}", case["name"].as_str().unwrap_or(""));
    let got = run(case);
    if let Some(want) = case.get("expect_error") {
        match got {
            Outcome::Error(c) => assert_eq!(&c, want.as_str().unwrap(), "{label}: {case}"),
            _ => panic!("{label}: expected the error {want}, it succeeded: {case}"),
        }
        return;
    }
    let want = &case["expect"];
    match got {
        Outcome::Text(t) => assert_eq!(&t, want.as_str().unwrap(), "{label}: {case}"),
        Outcome::Numeric(d) => assert_eq!(d, dec(want), "{label}: {case}"),
        Outcome::List(l) => {
            let want: Vec<&str> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(l, want, "{label}: {case}");
        }
        Outcome::Tax(t) => {
            assert_eq!(
                t.total.to_string(),
                want["total"].as_str().unwrap(),
                "{label} total: {case}"
            );
            let lines: Vec<String> = t.lines.iter().map(ToString::to_string).collect();
            let want_lines: Vec<&str> = want["lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(lines, want_lines, "{label} lines: {case}");
        }
        Outcome::Error(c) => panic!("{label}: unexpected error {c}: {case}"),
    }
}

#[test]
fn the_corpus_is_exact() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut total = 0;
    let mut files = 0;
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
    {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let cases: Vec<Value> = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        for (i, case) in cases.iter().enumerate() {
            check(&name, i, case);
        }
        total += cases.len();
        files += 1;
    }
    println!("{total} corpus cases in {files} files");
    assert!(
        files >= 8 && total >= 300,
        "the corpus must have at least 300 cases, found {total}"
    );
}

/// The corpus covers every mode and the scales 0 to 6, at positive, negative and exactly-half
/// values, so a mode cannot go untested by accident.
#[test]
fn the_corpus_covers_every_mode_and_scale() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let cases: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(dir.join("round_modes.json")).unwrap()).unwrap();
    for mode in [
        "half_up",
        "half_down",
        "half_even",
        "up",
        "down",
        "toward_zero",
        "away_from_zero",
    ] {
        for scale in [0, 1, 2, 3, 4, 6] {
            let of = |f: &dyn Fn(&str) -> bool| {
                cases
                    .iter()
                    .filter(|c| {
                        c["rule"]["mode"] == mode
                            && c["rule"]["scale"] == scale
                            && f(c["value"].as_str().unwrap())
                    })
                    .count()
            };
            assert!(
                of(&|v| v.starts_with('-')) > 0,
                "{mode} scale {scale}: no negative case"
            );
            assert!(
                of(&|v| !v.starts_with('-') && v != "0") > 0,
                "{mode} scale {scale}: no positive case"
            );
            // An exactly-half value: its last significant digit is 5 at one digit past the scale.
            assert!(
                of(&|v| v.trim_start_matches('-').ends_with('5')) > 0,
                "{mode} scale {scale}: no tie"
            );
        }
    }
}
