//! The publish path runs the publish guard before a version goes live (WP-BPM-04.T7): a version
//! with N violations is refused listing all N, nothing is registered, and a guard that cannot
//! answer refuses.
#![allow(clippy::unwrap_used, clippy::expect_used, deprecated)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_app_api::prelude::SiteId;
use spot_bpm_documents::service_principal::PUBLISH_PERMISSION;
use spot_bpm_documents::{MemoryDirectory, ProcessAuthor, PublishGuard, codes};
use spot_bpm_engine::{CaseId, Harness};
use spot_bpm_model::{ProcessVersion, from_text};
use spot_bpm_versioning::{PublishCheck, RefusedNode, VersioningError, publish, publish_guarded};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn site() -> SiteId {
    SiteId::new("bpm.test").unwrap()
}

fn alice() -> ProcessAuthor {
    ProcessAuthor::User("alice".to_owned())
}

/// A process of `version` whose Document nodes `a`, `b`... name the given principals.
fn process(version: u32, principals: &[Option<&str>]) -> ProcessVersion {
    let mut text = format!(
        "name = \"guarded\"\napp = \"versioning_test\"\nversion = {version}\nstatus = \"published\"\n\n\
[[nodes]]\nid = \"start\"\nkind = \"trigger\"\ntrigger = \"manual\"\n"
    );
    let mut previous = "start".to_owned();
    let mut edges = String::new();
    for (index, principal) in principals.iter().enumerate() {
        let id = format!("doc{index}");
        text.push_str(&format!(
            "\n[[nodes]]\nid = \"{id}\"\nkind = \"document\"\naction = \"post\"\ndoctype = \"Invoice\"\n"
        ));
        if let Some(principal) = principal {
            text.push_str(&format!("principal = \"{principal}\"\n"));
        }
        edges.push_str(&format!(
            "\n[[edges]]\nfrom = \"{previous}\"\nto = \"{id}\"\n"
        ));
        previous = id;
    }
    text.push_str(
        "\n[[nodes]]\nid = \"end\"\nkind = \"logic\"\nlogic = \"branch\"\nterminal = true\n",
    );
    text.push_str(&edges);
    text.push_str(&format!(
        "\n[[edges]]\nfrom = \"{previous}\"\nto = \"end\"\n"
    ));
    from_text(&text).expect("the process text must parse")
}

/// A directory where `svc_ledger` is registered and granted to alice, and alice holds the
/// publish permission.
fn directory() -> Arc<MemoryDirectory> {
    let directory = MemoryDirectory::new();
    directory.register(&site(), "svc_ledger");
    directory.grant(&site(), "svc_ledger", &alice());
    directory.allow_permission(&site(), &alice(), PUBLISH_PERMISSION);
    Arc::new(directory)
}

fn guard(directory: &Arc<MemoryDirectory>) -> PublishGuard {
    PublishGuard::new(directory.clone(), directory.clone())
}

fn run_guarded(
    harness: &Harness,
    version: &ProcessVersion,
    author: &ProcessAuthor,
    directory: &Arc<MemoryDirectory>,
) -> Result<spot_bpm_versioning::PublishReport, VersioningError> {
    let guard = guard(directory);
    let tenant = site();
    let check = PublishCheck {
        guard: &guard,
        tenant: &tenant,
        author,
    };
    publish_guarded(harness.engine(), version, &[], &check)
}

fn refused(error: VersioningError) -> Vec<RefusedNode> {
    match error {
        VersioningError::PublishRefused { violations } => violations,
        other => panic!("expected PublishRefused, got {other:?}"),
    }
}

#[test]
fn a_version_without_a_service_principal_publishes() {
    let harness = Harness::default();
    let report = run_guarded(&harness, &process(1, &[None, None]), &alice(), &directory()).unwrap();
    assert_eq!(report.published.version, 1);
    assert!(harness.engine().processes().get("guarded", 1).is_some());
}

#[test]
fn a_registered_granted_principal_publishes() {
    let harness = Harness::default();
    run_guarded(
        &harness,
        &process(1, &[Some("svc_ledger")]),
        &alice(),
        &directory(),
    )
    .unwrap();
    assert!(harness.engine().processes().get("guarded", 1).is_some());
}

#[test]
fn an_unregistered_principal_refuses() {
    let harness = Harness::default();
    let violations = refused(
        run_guarded(
            &harness,
            &process(1, &[Some("svc_other")]),
            &alice(),
            &directory(),
        )
        .unwrap_err(),
    );
    assert_eq!(
        violations,
        vec![RefusedNode {
            node: "doc0".to_owned(),
            rule: codes::PRINCIPAL_NOT_REGISTERED.to_owned(),
        }]
    );
    assert!(harness.engine().processes().get("guarded", 1).is_none());
}

#[test]
fn an_author_without_the_permission_refuses() {
    let harness = Harness::default();
    let bob = ProcessAuthor::User("bob".to_owned());
    let directory = directory();
    directory.grant(&site(), "svc_ledger", &bob);
    let violations = refused(
        run_guarded(
            &harness,
            &process(1, &[Some("svc_ledger")]),
            &bob,
            &directory,
        )
        .unwrap_err(),
    );
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].node, "doc0");
    assert_eq!(violations[0].rule, codes::PRINCIPAL_NOT_PERMITTED);
    assert!(harness.engine().processes().get("guarded", 1).is_none());
}

#[test]
fn two_violations_are_both_reported() {
    let harness = Harness::default();
    let violations = refused(
        run_guarded(
            &harness,
            &process(1, &[Some("svc_one"), Some("svc_ledger"), Some("svc_two")]),
            &alice(),
            &directory(),
        )
        .unwrap_err(),
    );
    let nodes: Vec<&str> = violations.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(nodes, vec!["doc0", "doc2"]);
    assert!(
        violations
            .iter()
            .all(|v| v.rule == codes::PRINCIPAL_NOT_REGISTERED)
    );
    assert!(harness.engine().processes().get("guarded", 1).is_none());
}

#[test]
fn a_refused_publish_leaves_the_registry_unchanged() {
    let harness = Harness::default();
    let directory = directory();
    run_guarded(&harness, &process(1, &[None]), &alice(), &directory).unwrap();
    let before = harness.engine().processes().latest("guarded").unwrap();

    let error = run_guarded(
        &harness,
        &process(2, &[Some("svc_other")]),
        &alice(),
        &directory,
    )
    .unwrap_err();
    assert_eq!(error.code(), "bpm_versioning.publish_refused");

    let after = harness.engine().processes().latest("guarded").unwrap();
    assert_eq!(after.number(), 1);
    assert_eq!(after.compiled.dag.hash(), before.compiled.dag.hash());
    assert!(harness.engine().processes().get("guarded", 2).is_none());
}

#[test]
fn a_guard_that_cannot_answer_refuses() {
    let harness = Harness::default();
    let directory = directory();
    directory.set_unreadable(true);
    let violations = refused(
        run_guarded(
            &harness,
            &process(1, &[Some("svc_ledger")]),
            &alice(),
            &directory,
        )
        .unwrap_err(),
    );
    assert_eq!(violations.len(), 1);
    assert!(harness.engine().processes().get("guarded", 1).is_none());
}

#[test]
fn the_unguarded_entry_point_refuses_a_principal_node() {
    let harness = Harness::default();
    let error = publish(
        harness.engine(),
        &process(1, &[None, Some("svc_ledger")]),
        &[],
    )
    .unwrap_err();
    assert_eq!(
        error,
        VersioningError::GuardRequired {
            node: "doc1".to_owned()
        }
    );
    assert_eq!(error.code(), "bpm_versioning.guard_required");
    assert!(harness.engine().processes().get("guarded", 1).is_none());
}

#[test]
fn the_unguarded_entry_point_still_accepts_a_version_without_a_principal() {
    let harness = Harness::default();
    let report = publish(harness.engine(), &process(1, &[None]), &[]).unwrap();
    assert_eq!(report.published.version, 1);
    assert!(harness.engine().processes().get("guarded", 1).is_some());
}

#[test]
fn running_pins_in_the_report_are_unchanged() {
    let harness = Harness::default();
    let directory = directory();
    let v1 = process(1, &[None]);
    let case = CaseId::new("pinned_case").unwrap();
    block_on(harness.start(&v1, case.clone(), "pin-key")).unwrap();
    let before = block_on(harness.engine().load(&case)).unwrap();

    let guard = guard(&directory);
    let tenant = site();
    let author = alice();
    let check = PublishCheck {
        guard: &guard,
        tenant: &tenant,
        author: &author,
    };
    let report = publish_guarded(
        harness.engine(),
        &process(2, &[None]),
        std::slice::from_ref(&before),
        &check,
    )
    .unwrap();
    assert_eq!(report.published.version, 2);
    assert_eq!(report.running.len(), 1);
    assert_eq!(report.running[0].pin.version, 1);
    assert_eq!(report.on_other_versions, vec![case]);
}
