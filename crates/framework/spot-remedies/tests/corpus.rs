//! The corpus: loader, validator and the deterministic matcher.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::path::Path;

use spot_remedies::{Corpus, Registry, RemedyError};

fn corpus() -> Corpus {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/cases");
    Corpus::from_dir(&dir).unwrap()
}

#[test]
fn the_seeded_corpus_loads_and_validates_against_the_registry() {
    let corpus = corpus();
    assert_eq!(corpus.len(), 7, "one case per seed remedy");
    let registry = Registry::seeds().unwrap();
    corpus.validate(&registry).unwrap();
}

#[test]
fn suggest_matches_evidence_deterministically() {
    let corpus = corpus();
    let symptoms = vec!["job.failed".to_owned(), "job.attempts_exhausted".to_owned()];
    let suggestions = corpus.suggest(&symptoms);
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].remedy.as_str(), "job.retry_failed");
    assert_eq!(suggestions[0].score, 2);
    assert_eq!(suggestions[0].cases, vec!["job-failed-retry".to_owned()]);

    // Same input, same order.
    assert_eq!(suggestions, corpus.suggest(&symptoms));

    // No matching evidence, no suggestion.
    assert!(corpus.suggest(&["nothing.matches".to_owned()]).is_empty());
}

#[test]
fn suggest_orders_by_score_then_remedy_id() {
    let corpus = corpus();
    // One signature matches two cases (quarantine and stream); another matches one.
    let suggestions = corpus.suggest(&[
        "quarantine.held".to_owned(),
        "stream.blocked".to_owned(),
        "case.stuck".to_owned(),
    ]);
    assert_eq!(suggestions[0].remedy.as_str(), "quarantine.retry_all");
    assert_eq!(suggestions[0].score, 2);
    assert_eq!(suggestions[1].remedy.as_str(), "case.unstick");
    assert_eq!(suggestions[1].score, 1);
}

#[test]
fn an_empty_symptom_is_refused() {
    let text = r#"
[[case]]
id = "bad"
symptom = ""
evidence = ["a.b"]
remedy = "job.retry_failed"
outcome = "resolved"
"#;
    let error = Corpus::from_toml(text).unwrap_err();
    assert!(
        matches!(error, RemedyError::CorpusInvalid { .. }),
        "got {error}"
    );
}

#[test]
fn a_case_without_evidence_is_refused() {
    let text = r#"
[[case]]
id = "bad"
symptom = "something"
evidence = []
remedy = "job.retry_failed"
outcome = "resolved"
"#;
    let error = Corpus::from_toml(text).unwrap_err();
    assert!(
        matches!(error, RemedyError::CorpusInvalid { .. }),
        "got {error}"
    );
}

#[test]
fn a_case_with_a_malformed_remedy_id_is_refused() {
    let text = r#"
[[case]]
id = "bad"
symptom = "something"
evidence = ["a.b"]
remedy = "JobRetry"
outcome = "resolved"
"#;
    let error = Corpus::from_toml(text).unwrap_err();
    assert!(
        matches!(error, RemedyError::CorpusInvalid { .. }),
        "got {error}"
    );
}

#[test]
fn a_case_with_an_unknown_outcome_is_refused() {
    let text = r#"
[[case]]
id = "bad"
symptom = "something"
evidence = ["a.b"]
remedy = "job.retry_failed"
outcome = "guessed"
"#;
    let error = Corpus::from_toml(text).unwrap_err();
    assert!(
        matches!(error, RemedyError::CorpusInvalid { .. }),
        "got {error}"
    );
}

#[test]
fn a_parameter_template_that_breaks_the_schema_is_refused() {
    // quarantine.retry_all takes no parameters, so any template key is invalid.
    let text = r#"
[[case]]
id = "bad-params"
symptom = "something"
evidence = ["a.b"]
remedy = "quarantine.retry_all"
outcome = "resolved"

[case.params]
unexpected = "value"
"#;
    let corpus = Corpus::from_toml(text).unwrap();
    let registry = Registry::seeds().unwrap();
    let error = corpus.validate(&registry).unwrap_err();
    assert!(
        matches!(error, RemedyError::CorpusInvalid { .. }),
        "got {error}"
    );
}
