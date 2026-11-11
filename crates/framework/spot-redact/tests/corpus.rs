#![allow(clippy::expect_used)]
// The brief exempts tests from the no-expect rule.
//! The corpus: every case in `tests/corpus/*.toml` must redact to its expected text.
//!
//! Failure output never prints case contents (registered-secret material lives in the
//! known-secret corpus data): only file, index and class — and, for pattern classes when
//! `SPOT_REDACT_CORPUS_DEBUG=1`, the diff.

use serde::Deserialize;
use spot_redact::Redactor;
use spot_secrets::SecretString;

/// Fake secrets for the corpus only; nothing real, never printed in failure output.
const DEMO_SECRETS: [(&str, &str); 3] = [
    ("staging_db", "hunter2staging"),
    ("example", "correct-horse-battery"),
    ("legacy_key", "DbP4ss&/word9x"),
];

/// The redactor under test: every rule on (including `pii`), plus the demo secrets.
fn test_redactor() -> Redactor {
    let mut policy = spot_redact::RedactionPolicy::default();
    policy.patterns.pii_email = true;
    policy.patterns.pii_phone = true;
    let mut redactor = Redactor::from_policy(&policy).expect("the test policy parses");
    for (label, value) in DEMO_SECRETS {
        let registered = redactor.register_secret(label, &SecretString::new(value));
        assert!(
            registered.is_ok(),
            "demo secret registration failed for label {label}"
        );
    }
    redactor
}

#[derive(Deserialize)]
struct CorpusCase {
    input: String,
    expected: String,
    class: String,
}

#[derive(Deserialize)]
struct Corpus {
    #[serde(rename = "case")]
    cases: Vec<CorpusCase>,
}

#[test]
fn every_corpus_case_redacts_to_its_expected_text() {
    let redactor = test_redactor();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
    let mut files: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(dir)
        .expect("the corpus directory exists")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.to_owned();
            name.ends_with(".toml").then_some((name, path))
        })
        .collect();
    files.sort();
    assert!(
        files.len() >= 10,
        "the corpus holds at least ten class files"
    );
    let mut total = 0usize;
    for (name, path) in files {
        let text = std::fs::read_to_string(&path).expect("the corpus file reads");
        let corpus: Corpus = toml::from_str(&text).expect("each corpus file parses");
        let class = name.trim_end_matches(".toml").to_owned();
        for (i, case) in corpus.cases.iter().enumerate() {
            assert_eq!(
                case.class, class,
                "file {name} case {i}: class matches the file stem"
            );
            let actual = redactor.redact_str(&case.input);
            let ok = actual == case.expected;
            if !ok {
                // registered-secret material must never reach a failure message
                if case.class != "known_secret"
                    && std::env::var("SPOT_REDACT_CORPUS_DEBUG").as_deref() == Ok("1")
                {
                    eprintln!(
                        "diff for {name}[{i}]:\n  input:    {:?}\n  expected: {:?}\n  actual:   {:?}",
                        case.input, case.expected, actual
                    );
                } else {
                    eprintln!("corpus mismatch at {name}[{i}] (class {})", case.class);
                }
            }
            assert!(ok, "corpus case failed: {name}[{i}] (class {})", case.class);
            total += 1;
        }
    }
    eprintln!("corpus cases total: {total}");
    assert!(total >= 150, "the corpus holds at least 150 cases");
}
