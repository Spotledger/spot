//! Each action, the dry run, the refusals and the non-leak rule.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;

use common::{DOCTYPE, Fake, invocation, rid};
use serde_json::json;
use spot_app_api::document::{DocState, DocumentPatch};
use spot_app_api::prelude::{Name, Value, Version};
use spot_bulk_actions::{
    BulkAction, BulkOptions, BulkOutcome, BulkStatus, BulkTarget, CancellationToken, NoProgress,
    SkipReason, apply_bulk, codes,
};

fn targets(range: std::ops::RangeInclusive<u8>) -> Vec<BulkTarget> {
    range
        .map(|n| BulkTarget::current(DOCTYPE, rid(n)))
        .collect()
}

async fn run(
    fake: &Fake,
    action: &BulkAction,
    list: Vec<BulkTarget>,
    options: &BulkOptions,
) -> BulkOutcome {
    apply_bulk(
        fake,
        &invocation(),
        action,
        list,
        options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn patch() -> DocumentPatch {
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(Name::new("remarks").unwrap(), Value::Text("x".to_owned()));
    patch
}

fn code(status: &BulkStatus) -> String {
    match status {
        BulkStatus::Failed { code, .. } => code.clone(),
        other => panic!("not failed: {other:?}"),
    }
}

#[tokio::test]
async fn update_cancel_correct_delete_and_method_each_report_their_version() {
    let fake = Fake::plain();
    fake.add(1, Some(DocState::Draft)); // update
    fake.add(2, Some(DocState::Posted)); // cancel
    fake.add(3, Some(DocState::Posted)); // correct
    fake.add(4, Some(DocState::Draft)); // delete
    fake.add(5, Some(DocState::Draft)); // method
    let options = BulkOptions::new("actions");

    let r = run(
        &fake,
        &BulkAction::Update(patch()),
        targets(1..=1),
        &options,
    )
    .await;
    assert_eq!(
        r.results[0].status,
        BulkStatus::Succeeded {
            version: Some(Version(2))
        }
    );
    let r = run(&fake, &BulkAction::Cancel, targets(2..=2), &options).await;
    // The document was at version 1 (every double document starts there): the cancel writes 2.
    assert_eq!(
        r.results[0].status,
        BulkStatus::Succeeded {
            version: Some(Version(2))
        }
    );
    let r = run(&fake, &BulkAction::Correct, targets(3..=3), &options).await;
    // The new draft's version.
    assert_eq!(
        r.results[0].status,
        BulkStatus::Succeeded {
            version: Some(Version(1))
        }
    );
    assert_eq!(fake.state_of(rid(3)), Some(DocState::Cancelled));
    let r = run(&fake, &BulkAction::Delete, targets(4..=4), &options).await;
    assert_eq!(r.results[0].status, BulkStatus::Succeeded { version: None });
    assert!(!fake.exists(rid(4)));
    let method = BulkAction::Method {
        name: "mark_reviewed".to_owned(),
        args: json!({"by": "x"}),
    };
    let r = run(&fake, &method, targets(5..=5), &options).await;
    assert_eq!(
        r.results[0].status,
        BulkStatus::Succeeded {
            version: Some(Version(2))
        }
    );
}

#[tokio::test]
async fn refused_writes_are_per_document_failures() {
    let fake = Fake::plain();
    fake.add(1, Some(DocState::Posted));
    fake.add(2, Some(DocState::Cancelled));
    let options = BulkOptions::new("refuse");
    // Delete of a posted document.
    let r = run(&fake, &BulkAction::Delete, targets(1..=1), &options).await;
    assert_eq!(code(&r.results[0].status), "document.invalid_state");
    assert!(fake.exists(rid(1)));
    // Update of a cancelled document (the double's stand-in for a validation refusal): the
    // engine's code comes back untouched.
    let r = run(
        &fake,
        &BulkAction::Update(patch()),
        targets(2..=2),
        &options,
    )
    .await;
    assert_eq!(code(&r.results[0].status), "document.invalid_state");
}

#[tokio::test]
async fn refusals_make_no_call() {
    let fake = Fake::plain();
    fake.add_drafts(1..=6);
    let mut options = BulkOptions::new("limits");
    options.max_targets = 5;
    let err = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        targets(1..=6),
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), codes::TOO_MANY_TARGETS);

    let cases: Vec<(BulkAction, Vec<BulkTarget>, BulkOptions, &str)> = vec![
        (
            BulkAction::Post,
            Vec::new(),
            BulkOptions::new("ok"),
            codes::EMPTY_BATCH,
        ),
        (
            BulkAction::Post,
            targets(1..=1),
            BulkOptions::new(""),
            codes::BAD_LABEL,
        ),
        (
            BulkAction::Post,
            targets(1..=1),
            BulkOptions::new(&"x".repeat(65)),
            codes::BAD_LABEL,
        ),
        (
            BulkAction::Post,
            targets(1..=1),
            BulkOptions::new("bad\nlabel"),
            codes::BAD_LABEL,
        ),
        (
            BulkAction::Post,
            targets(1..=1),
            {
                let mut o = BulkOptions::new("ok");
                o.concurrency = 0;
                o
            },
            codes::BAD_CONCURRENCY,
        ),
        (
            BulkAction::Post,
            targets(1..=1),
            {
                let mut o = BulkOptions::new("ok");
                o.concurrency = 17;
                o
            },
            codes::BAD_CONCURRENCY,
        ),
        (
            BulkAction::Method {
                name: String::new(),
                args: json!(null),
            },
            targets(1..=1),
            BulkOptions::new("ok"),
            codes::ACTION_NOT_ALLOWED,
        ),
        (
            BulkAction::Method {
                name: "Bad-Name".to_owned(),
                args: json!(null),
            },
            targets(1..=1),
            BulkOptions::new("ok"),
            codes::ACTION_NOT_ALLOWED,
        ),
        (
            BulkAction::Update(DocumentPatch::default()),
            targets(1..=1),
            BulkOptions::new("ok"),
            codes::ACTION_NOT_ALLOWED,
        ),
    ];
    for (action, list, options, want) in cases {
        let err = apply_bulk(
            &fake,
            &invocation(),
            &action,
            list,
            &options,
            &NoProgress,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), want);
    }
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fake.loads.load(Ordering::SeqCst), 0);
}

fn seeded(panic_on_write: bool) -> Fake {
    let mut fake = Fake::plain();
    fake.panic_on_write = panic_on_write;
    fake.add_drafts(1..=3); // would post
    fake.add(4, Some(DocState::Posted)); // already posted
    fake.add(5, Some(DocState::Cancelled)); // cannot be posted
    fake.add(6, Some(DocState::Draft)); // stale expected
    fake
}

fn mixed_targets() -> Vec<BulkTarget> {
    let mut list = targets(1..=5);
    list.push(BulkTarget {
        doctype: DOCTYPE.to_owned(),
        id: rid(6),
        expected: Some(Version(9)),
    });
    list.push(BulkTarget::current(DOCTYPE, rid(7))); // missing
    list
}

#[tokio::test]
async fn dry_run_writes_nothing_and_matches_what_the_real_run_then_does() {
    let mut options = BulkOptions::new("dry");
    options.dry_run = true;
    let dry_fake = seeded(true); // panics on any write
    let dry = run(&dry_fake, &BulkAction::Post, mixed_targets(), &options).await;
    assert!(dry.dry_run);
    assert_eq!(dry.would_succeed, 3);
    assert_eq!(dry.succeeded, 0);
    assert_eq!(dry.skipped, 1);
    assert_eq!(dry.failed, 3);
    assert_eq!(dry_fake.writes.load(Ordering::SeqCst), 0);

    options.dry_run = false;
    let real_fake = seeded(false);
    let real = run(&real_fake, &BulkAction::Post, mixed_targets(), &options).await;
    for (d, r) in dry.results.iter().zip(&real.results) {
        match (&d.status, &r.status) {
            (BulkStatus::WouldSucceed, BulkStatus::Succeeded { .. }) => {}
            (BulkStatus::Skipped { reason: a }, BulkStatus::Skipped { reason: b }) => {
                assert_eq!(a, b);
            }
            (BulkStatus::Failed { code: a, .. }, BulkStatus::Failed { code: b, .. }) => {
                assert_eq!(a, b);
            }
            other => panic!("dry and real differ: {other:?}"),
        }
    }
    assert_eq!(
        dry.results[3].status,
        BulkStatus::Skipped {
            reason: SkipReason::AlreadyPosted
        }
    );
}

#[tokio::test]
async fn a_failure_message_does_not_tell_a_hidden_document_from_a_missing_one() {
    let fake = Fake::plain();
    fake.hide(1);
    let r = run(
        &fake,
        &BulkAction::Post,
        vec![
            BulkTarget::current(DOCTYPE, rid(1)),
            BulkTarget::current(DOCTYPE, rid(2)),
        ],
        &BulkOptions::new("leak"),
    )
    .await;
    let (hidden, missing) = (&r.results[0].status, &r.results[1].status);
    assert_eq!(hidden, missing);
    if let BulkStatus::Failed { message, .. } = hidden {
        assert!(!message.contains(&rid(1).to_string()));
        assert!(!message.contains(&rid(2).to_string()));
    } else {
        panic!("expected a failure");
    }
}
