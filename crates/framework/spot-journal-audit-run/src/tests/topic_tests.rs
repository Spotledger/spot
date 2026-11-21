//! The classifier and the unregistered-topic policy of a scheduled audit.
//!
//! The journal holds a state entry, a domain event (`doc_posted`), an identity audit topic
//! (`identity_role_changed`) and one application topic (`acme_note_added`) the platform registry
//! does not know.

use spot_image_upcast::Registry;
use spot_journal_rebuild::{EventClass, EventClassifier, RegistryClassifier};

use super::block_on;
use super::fixture::{Journal, TENANT, put, topic};
use crate::memory::{MemoryScratch, SteppingClock};
use crate::{
    AuditOptions, AuditRun, ClassifierError, ClassifierKind, DomainTopicsClassifier, FailureStage,
    MAX_REPORTED_TOPICS, REBUILD_UNKNOWN_TOPIC, UnknownTopics, run_audit,
};

const APP_TOPIC: &str = "acme_note_added";

fn journal() -> Journal {
    Journal::new(
        vec![
            put(1, 1),
            topic("doc_posted", 1),
            topic("identity_role_changed", 1),
            topic(APP_TOPIC, 1),
            put(2, 1),
        ],
        2,
    )
}

fn audit(journal: &Journal, options: &AuditOptions<'_>) -> (AuditRun, MemoryScratch) {
    let live = journal.live();
    let mut scratch = MemoryScratch::new(TENANT);
    let run = block_on(run_audit(
        &journal.store,
        &journal.keys(),
        &Registry::new(),
        &live,
        &mut scratch,
        options,
        &SteppingClock::new(10_000, 5),
    ))
    .unwrap();
    (run, scratch)
}

#[test]
fn the_default_fails_at_the_rebuild_with_a_stable_code_and_verifies_nothing() {
    let journal = journal();
    let options = journal.options(1_000);
    assert!(options.classifier.is_none());
    assert!(options.strict_topics && !options.allow_unregistered);
    let (run, scratch) = audit(&journal, &options);
    let AuditRun::Failed(failure) = &run else {
        panic!("an unknown topic must fail the rebuild stage: {run:?}");
    };
    assert_eq!(failure.stage, FailureStage::Rebuild);
    assert_eq!(failure.code, REBUILD_UNKNOWN_TOPIC);
    assert_eq!(failure.code, "journal_audit_run.rebuild_unknown_topic");
    assert_eq!(failure.unknown_topic.as_deref(), Some(APP_TOPIC));
    assert!(run.verified_position().is_none());
    assert!(!run.is_clean());
    assert_eq!(scratch.discard_count(), 1, "the scratch side is discarded");
}

#[test]
fn an_injected_classifier_that_declares_the_app_topic_audits_the_tenant() {
    let journal = journal();
    let classifier =
        DomainTopicsClassifier::new(RegistryClassifier, &[(APP_TOPIC, EventClass::Domain)])
            .unwrap();
    let mut options = journal.options(1_000);
    options.classifier = Some(&classifier);
    let (run, _) = audit(&journal, &options);
    let report = run.report().expect("the audit completed");
    assert!(run.is_clean(), "{run:?}");
    assert_eq!(report.classifier, ClassifierKind::Injected);
    assert_eq!(report.classifier.name(), "injected");
    assert!(
        report.unknown_topics.is_empty(),
        "{:?}",
        report.unknown_topics
    );
    assert!(!report.policy_relaxed);
    assert_eq!(
        report.rebuild.domain_skipped, 2,
        "doc_posted and the app topic"
    );
    assert_eq!(report.rebuild.identity_audit_applied, 1);
    assert!(run.verified_position().is_some());
}

#[test]
fn a_relaxed_policy_completes_and_lists_the_unknown_topic() {
    let journal = journal();
    let mut options = journal.options(1_000);
    options.strict_topics = false;
    let (run, _) = audit(&journal, &options);
    let report = run.report().expect("the audit completed");
    assert_eq!(report.classifier, ClassifierKind::Registry);
    assert_eq!(report.classifier.name(), "registry");
    assert!(report.policy_relaxed, "the relaxed policy is flagged");
    assert_eq!(report.unknown_topics.names, vec![APP_TOPIC.to_owned()]);
    assert_eq!(report.unknown_topics.invalid, 0);
    assert_eq!(report.rebuild.classes.get("unknown"), Some(&1));
    assert!(
        run.is_clean(),
        "an unknown topic is domain and changes no record"
    );
}

#[test]
fn the_report_carries_no_unknown_topic_when_the_journal_holds_none() {
    let journal = Journal::new(vec![put(1, 1), topic("doc_posted", 1), put(2, 1)], 2);
    let (run, _) = audit(&journal, &journal.options(1_000));
    let report = run.report().unwrap();
    assert!(report.unknown_topics.is_empty());
    assert_eq!(report.classifier, ClassifierKind::Registry);
    assert!(!report.policy_relaxed);
}

#[test]
fn a_declared_topic_may_not_shadow_a_platform_topic() {
    let error =
        DomainTopicsClassifier::new(RegistryClassifier, &[("doc_posted", EventClass::State)])
            .unwrap_err();
    assert_eq!(
        error,
        ClassifierError::ShadowsRegistered {
            topic: "doc_posted".to_owned()
        }
    );
    assert_eq!(
        error.code(),
        "journal_audit_run.classifier_shadows_registered"
    );
    assert!(error.to_string().contains("doc_posted"));
}

#[test]
fn a_declared_topic_must_be_a_valid_app_topic_and_declared_once() {
    for bad in [
        "Bad Topic",
        "",
        "sys_unregistered_thing",
        "identity_made_up",
    ] {
        let error = DomainTopicsClassifier::new(RegistryClassifier, &[(bad, EventClass::Domain)])
            .unwrap_err();
        assert_eq!(error, ClassifierError::InvalidTopic, "{bad}");
        assert!(!error.to_string().contains(bad) || bad.is_empty());
    }
    let error = DomainTopicsClassifier::new(
        RegistryClassifier,
        &[
            (APP_TOPIC, EventClass::Domain),
            (APP_TOPIC, EventClass::Audit),
        ],
    )
    .unwrap_err();
    assert!(matches!(error, ClassifierError::Conflicting { .. }));
    // The same topic twice with the same class is harmless.
    assert!(
        DomainTopicsClassifier::new(
            RegistryClassifier,
            &[
                (APP_TOPIC, EventClass::Domain),
                (APP_TOPIC, EventClass::Domain)
            ],
        )
        .is_ok()
    );
}

#[test]
fn the_classifier_answers_the_registry_first_and_none_for_an_unknown_topic() {
    let classifier =
        DomainTopicsClassifier::new(RegistryClassifier, &[(APP_TOPIC, EventClass::Audit)]).unwrap();
    assert_eq!(classifier.class_of("doc_state"), Some(EventClass::State));
    assert_eq!(classifier.class_of("doc_posted"), Some(EventClass::Domain));
    assert_eq!(classifier.class_of(APP_TOPIC), Some(EventClass::Audit));
    assert_eq!(classifier.class_of("acme_never_declared"), None);
}

#[test]
fn the_unknown_topic_list_is_capped_sorted_and_never_echoes_an_invalid_name() {
    let names: Vec<String> = (0..60).map(|n| format!("app_topic_{n:03}")).collect();
    let mut all: Vec<&str> = names.iter().map(String::as_str).collect();
    all.push("Not A Topic: SECRET-VALUE");
    all.push("app_topic_000");
    let topics = UnknownTopics::from_names(all, true);
    assert_eq!(topics.names.len(), MAX_REPORTED_TOPICS);
    assert_eq!(topics.names[0], "app_topic_000");
    assert_eq!(topics.more, 10);
    assert_eq!(topics.invalid, 1);
    assert!(topics.truncated);
    assert_eq!(topics.total(), 61);
    assert!(!format!("{topics:?}").contains("SECRET-VALUE"));
}
