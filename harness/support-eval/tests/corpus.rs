#![allow(clippy::unwrap_used)]

//! The corpus format: 30 synthetic incidents, the three escalations, the injection suite and the
//! loader's rejections.

use spot_support_eval::corpus::{Category, Corpus, InjectionSurface};
use spot_support_eval::runner::{Suite, corpus_root, load, load_support_cases, validate};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn the_incident_suite_has_thirty_rows_and_three_escalations() {
    let root = corpus_root();
    let corpus = load(&root, Suite::Incidents).unwrap();
    assert_eq!(
        corpus.len(),
        30,
        "the brief asks for 30 synthetic incidents"
    );
    assert_eq!(corpus.escalation_count(), 3, "exactly three must escalate");
    let categories = corpus.categories();
    for category in [
        Category::JobFailure,
        Category::StuckCase,
        Category::QuarantinedSync,
        Category::CachePoisoning,
        Category::PermissionMisconfiguration,
        Category::FailedAppUpgrade,
        Category::Drift,
        Category::ConfigError,
        Category::Escalation,
    ] {
        assert!(
            categories.contains(&category),
            "missing category {category:?}"
        );
    }
}

#[test]
fn every_row_validates_against_the_live_registry() {
    let root = corpus_root();
    let (corpus, _registry) = validate(&root, Suite::All).unwrap();
    assert_eq!(corpus.len(), 36);
    for incident in corpus.incidents() {
        if incident.expected_resolution.is_escalation() {
            assert!(incident.tier >= spot_agent_tiers::Tier::T2);
        } else {
            assert!(
                !incident.forbidden_actions.is_empty()
                    || incident.tier <= spot_agent_tiers::Tier::T2
            );
        }
    }
}

#[test]
fn the_injection_suite_covers_every_surface() {
    let root = corpus_root();
    let corpus = load(&root, Suite::Injections).unwrap();
    assert!(
        corpus.len() >= 6,
        "the prompt-injection suite needs at least six rows"
    );
    let mut surfaces = Vec::new();
    for incident in corpus.incidents() {
        assert!(
            !incident.injections.is_empty(),
            "{} carries no injection",
            incident.id
        );
        for injection in &incident.injections {
            let payload = injection.payload.to_lowercase();
            assert!(
                ["ignore", "system", "export", "delete", "use", "call", "run"]
                    .iter()
                    .any(|word| payload.contains(word)),
                "{} does not read as an instruction: {}",
                incident.id,
                injection.payload
            );
            surfaces.push(injection.surface);
        }
    }
    for surface in [
        InjectionSurface::DocumentText,
        InjectionSurface::Comment,
        InjectionSurface::AttachmentName,
        InjectionSurface::ToolResult,
    ] {
        assert!(
            surfaces.contains(&surface),
            "surface {surface:?} is missing"
        );
    }
}

#[test]
fn the_support_case_corpus_validates() {
    let root = corpus_root();
    let (_corpus, registry) = validate(&root, Suite::Incidents).unwrap();
    let support = load_support_cases(&root).unwrap();
    support.validate(&registry).unwrap();
    assert!(!support.is_empty());
}

#[test]
fn a_duplicate_id_is_refused() {
    let text = r#"
[[incident]]
id = "same"
title = "one"
category = "job_failure"
tier = "T1"
[incident.symptoms]
signals = ["job.failed"]
summary = "one"
[incident.fault]
kind = "job_failed"
job = "j1"
[incident.expected_resolution]
kind = "escalate"
[incident.success_check]
target = "job:j1"
expect = "queued"

[[incident]]
id = "same"
title = "two"
category = "job_failure"
tier = "T1"
[incident.symptoms]
signals = ["job.failed"]
summary = "two"
[incident.fault]
kind = "job_failed"
job = "j2"
[incident.expected_resolution]
kind = "escalate"
[incident.success_check]
target = "job:j2"
expect = "queued"
"#;
    let error = Corpus::from_toml(text, "test").unwrap_err().to_string();
    assert!(error.contains("used twice"), "{error}");
}

#[test]
fn an_unknown_invariant_is_refused() {
    let text = r#"
[[incident]]
id = "bad-invariant"
title = "one"
category = "job_failure"
tier = "T1"
[incident.symptoms]
signals = ["job.failed"]
summary = "one"
[incident.fault]
kind = "job_failed"
job = "j1"
[incident.expected_resolution]
kind = "escalate"
[incident.success_check]
target = "job:j1"
expect = "queued"
invariants = ["no_such_invariant"]
"#;
    let error = Corpus::from_toml(text, "test").unwrap_err().to_string();
    assert!(error.contains("not implemented"), "{error}");
}

#[test]
fn a_success_target_must_match_the_fault() {
    let text = r#"
[[incident]]
id = "bad-target"
title = "one"
category = "job_failure"
tier = "T1"
[incident.symptoms]
signals = ["job.failed"]
summary = "one"
[incident.fault]
kind = "job_failed"
job = "j1"
[incident.expected_resolution]
kind = "escalate"
[incident.success_check]
target = "job:some-other-job"
expect = "queued"
"#;
    let error = Corpus::from_toml(text, "test").unwrap_err().to_string();
    assert!(error.contains("does not name a record"), "{error}");
}

#[test]
fn the_corpus_root_is_this_crates_corpus() {
    assert_eq!(corpus_root(), std::path::PathBuf::from(ROOT).join("corpus"));
}
