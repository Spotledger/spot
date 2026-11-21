//! Per-tenant classifiers in the scheduled audit.
//!
//! Three tenants each hold one application topic the platform registry does not know. Two have a
//! classifier that declares their own topic (and only that one); the third has none and so falls
//! back to the strict registry default.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_conditions::{ConditionEngine, MemorySink};
use spot_journal_audit_run::{
    ClassifierKind, DomainTopicsClassifier, EventClass, EventClassifier, MAX_REPORTED_TOPICS,
    REBUILD_UNKNOWN_TOPIC, RegistryClassifier,
};

use super::fixture::{Journal, engine, runtime};
use crate::memory::{FnInputs, MemoryBook, SharedClock};
use crate::{
    AuditJobError, Cadence, JOURNAL_AUDIT_FAILED, JOURNAL_AUDIT_MISMATCH, REASON_UNKNOWN_TOPICS,
    RunSummary, TenantOutcome, run_due,
};

const NOW: i64 = 1_800_000_000_000;

fn classifier_for(topic: &str) -> Arc<dyn EventClassifier + Send + Sync> {
    Arc::new(
        DomainTopicsClassifier::new(RegistryClassifier, &[(topic, EventClass::Domain)]).unwrap(),
    )
}

/// The inputs opener the tests hand to the job.
type OpenFn = Box<
    dyn Fn(&spot_journal_store::TenantId) -> Result<crate::TenantInputs, AuditJobError>
        + Send
        + Sync,
>;

struct Env {
    rt: tokio::runtime::Runtime,
    book: Arc<MemoryBook>,
    engine: ConditionEngine<MemorySink>,
    clock: SharedClock,
    inputs: FnInputs<OpenFn>,
}

/// `(tenant, topic in its journal, topic its classifier declares or None, strict policy)`.
fn env(tenants: &[(&'static str, &'static str, Option<&'static str>, bool)]) -> Env {
    let rt = runtime();
    let book = Arc::new(MemoryBook::new());
    let mut journals = BTreeMap::new();
    for (tenant, topic, declares, strict) in tenants {
        book.add(tenant, Cadence::Daily, None);
        journals.insert(
            (*tenant).to_owned(),
            (
                Arc::new(Journal::with_topic(&rt, tenant, 3, topic)),
                *declares,
                *strict,
            ),
        );
    }
    let open: OpenFn = Box::new(move |tenant| {
        let (journal, declares, strict) = journals.get(tenant.as_str()).unwrap();
        Ok(journal.inputs_with(journal.live(), false, declares.map(classifier_for), *strict))
    });
    Env {
        rt,
        book,
        engine: engine(),
        clock: SharedClock::new(NOW, 5),
        inputs: FnInputs::new(open),
    }
}

impl Env {
    fn run(&mut self) -> RunSummary {
        run_due(
            self.rt.handle(),
            NOW,
            self.book.as_ref(),
            &self.inputs,
            &self.clock,
            &mut self.engine,
            self.book.as_ref(),
        )
        .unwrap()
    }

    fn open(&self) -> Vec<(String, String)> {
        self.engine
            .open()
            .iter()
            .map(|o| (o.id.as_str().to_owned(), o.subject.as_str().to_owned()))
            .collect()
    }
}

#[test]
fn a_tenant_with_its_classifier_is_audited_and_a_tenant_without_one_fails_on_unknown_topics() {
    let mut env = env(&[
        ("acme", "acme_note_added", Some("acme_note_added"), true),
        ("beta", "beta_note_added", None, true),
        ("gamma", "gamma_note_added", Some("gamma_note_added"), true),
    ]);
    let summary = env.run();
    assert_eq!(summary.results.len(), 3, "the other tenants still run");

    for tenant in ["acme", "gamma"] {
        let result = summary.result(tenant).unwrap();
        assert_eq!(result.outcome, TenantOutcome::Clean, "{tenant}");
        assert_eq!(result.classifier, ClassifierKind::Injected);
        assert!(result.unknown_topics.is_empty());
        assert!(result.verified_position().is_some());
    }

    let beta = summary.result("beta").unwrap();
    let TenantOutcome::Failed(info) = beta.outcome else {
        panic!("beta must fail: {:?}", beta.outcome);
    };
    assert_eq!(info.stage, "rebuild");
    assert_eq!(info.code, REBUILD_UNKNOWN_TOPIC);
    assert_eq!(info.reason, REASON_UNKNOWN_TOPICS);
    assert_eq!(beta.classifier, ClassifierKind::Registry);
    assert_eq!(
        beta.unknown_topics.names,
        vec!["beta_note_added".to_owned()]
    );
    assert!(beta.unknown_topics.names.len() <= MAX_REPORTED_TOPICS);
    assert!(beta.verified_position().is_none());
    assert_eq!(env.book.results().len(), 3);
}

#[test]
fn an_unknown_topic_raises_the_failed_condition_with_its_reason_and_never_the_mismatch() {
    let mut env = env(&[
        ("acme", "acme_note_added", Some("acme_note_added"), true),
        ("beta", "beta_note_added", None, true),
    ]);
    env.run();
    assert_eq!(
        env.open(),
        vec![(JOURNAL_AUDIT_FAILED.to_owned(), "beta".to_owned())]
    );
    let open = env.engine.open();
    let raised = &open[0];
    assert_eq!(raised.params.get("stage"), Some("rebuild"));
    assert_eq!(raised.params.get("code"), Some(REBUILD_UNKNOWN_TOPIC));
    assert_eq!(raised.params.get("reason"), Some("unknown_topics"));
    assert_eq!(raised.params.get("topic_count"), Some("1"));
    assert!(
        env.open()
            .iter()
            .all(|(id, _)| id != JOURNAL_AUDIT_MISMATCH),
        "the data was not compared, so nothing mismatched"
    );
}

#[test]
fn a_classifier_given_for_one_tenant_is_never_used_for_another() {
    // `acme`'s classifier declares only acme's topic; `beta`'s journal holds beta's. If the
    // classifiers crossed, one of the two would fail.
    let mut env = env(&[
        ("acme", "acme_note_added", Some("acme_note_added"), true),
        ("beta", "beta_note_added", Some("beta_note_added"), true),
    ]);
    let summary = env.run();
    assert_eq!(
        summary.result("acme").unwrap().outcome,
        TenantOutcome::Clean
    );
    assert_eq!(
        summary.result("beta").unwrap().outcome,
        TenantOutcome::Clean
    );

    // The reverse wiring: each tenant given the other's classifier fails.
    let mut env = env_crossed();
    let summary = env.run();
    for tenant in ["acme", "beta"] {
        let TenantOutcome::Failed(info) = summary.result(tenant).unwrap().outcome else {
            panic!("{tenant} must fail with the other tenant's classifier");
        };
        assert_eq!(info.code, REBUILD_UNKNOWN_TOPIC);
    }
}

fn env_crossed() -> Env {
    let mut env = env(&[]);
    let rt = &env.rt;
    let journals: BTreeMap<String, Arc<Journal>> =
        [("acme", "acme_note_added"), ("beta", "beta_note_added")]
            .iter()
            .map(|(tenant, topic)| {
                env.book.add(tenant, Cadence::Daily, None);
                (
                    (*tenant).to_owned(),
                    Arc::new(Journal::with_topic(rt, tenant, 3, topic)),
                )
            })
            .collect();
    let open: OpenFn = Box::new(move |tenant| {
        let journal = journals.get(tenant.as_str()).unwrap();
        let other = if tenant.as_str() == "acme" {
            "beta_note_added"
        } else {
            "acme_note_added"
        };
        Ok(journal.inputs_with(journal.live(), false, Some(classifier_for(other)), true))
    });
    env.inputs = FnInputs::new(open);
    env
}

#[test]
fn a_relaxed_tenant_is_audited_and_lists_its_unknown_topic() {
    let mut env = env(&[("acme", "acme_note_added", None, false)]);
    let summary = env.run();
    let result = summary.result("acme").unwrap();
    assert_eq!(result.outcome, TenantOutcome::Clean);
    assert_eq!(result.classifier, ClassifierKind::Registry);
    assert_eq!(
        result.unknown_topics.names,
        vec!["acme_note_added".to_owned()]
    );
    assert!(
        result
            .run
            .as_ref()
            .and_then(|run| run.report())
            .is_some_and(|r| r.policy_relaxed),
        "the report says the policy was relaxed"
    );
    assert!(env.open().is_empty());
}
