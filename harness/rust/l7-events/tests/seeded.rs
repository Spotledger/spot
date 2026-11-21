//! A green run must be able to turn red: each seeded failure is caught by exactly the invariant
//! meant to catch it, and the clean outbox is caught by none.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_l7_events::expected_for_engine_world;
use spot_l7_events::{COVERAGE, REGISTERED, SCHEMA, SECRETS};
use spot_storage_contract::RecordId;
use support::{clean_outbox, failed_on, record};

#[tokio::test]
async fn the_clean_outbox_fails_nothing() {
    let failed = failed_on(
        clean_outbox(),
        vec!["l7-secret-1".into()],
        expected_for_engine_world(),
    )
    .await;
    assert!(failed.is_empty(), "{failed:?}");
}

#[tokio::test]
async fn an_unknown_platform_topic_is_caught_by_registered_only() {
    let mut outbox = clean_outbox();
    outbox.push(record(
        99,
        "doc_exploded",
        RecordId::new_v7(),
        Some(1),
        serde_json::json!({"name": "a"}),
    ));
    assert_eq!(
        failed_on(outbox, vec![], expected_for_engine_world()).await,
        [REGISTERED]
    );
}

#[tokio::test]
async fn a_payload_field_of_the_wrong_type_is_caught_by_the_schema_only() {
    let mut outbox = clean_outbox();
    outbox.push(record(
        99,
        "doc_updated",
        RecordId::new_v7(),
        Some(9),
        serde_json::json!({"name": "a", "changed_fields": "subject"}),
    ));
    assert_eq!(
        failed_on(outbox, vec![], expected_for_engine_world()).await,
        [SCHEMA]
    );
}

#[tokio::test]
async fn an_extra_field_the_strict_schema_forbids_is_caught_by_the_schema_only() {
    let mut outbox = clean_outbox();
    outbox.push(record(
        99,
        "doc_created",
        RecordId::new_v7(),
        Some(1),
        serde_json::json!({"name": "a", "extra": 1}),
    ));
    assert_eq!(
        failed_on(outbox, vec![], expected_for_engine_world()).await,
        [SCHEMA]
    );
}

#[tokio::test]
async fn a_secret_value_copied_into_a_payload_is_caught_by_the_secret_check_only() {
    let mut outbox = clean_outbox();
    outbox.push(record(
        99,
        "doc_created",
        RecordId::new_v7(),
        Some(1),
        serde_json::json!({"name": "l7-secret-9"}),
    ));
    let failed = failed_on(
        outbox,
        vec!["l7-secret-9".into()],
        expected_for_engine_world(),
    )
    .await;
    assert_eq!(failed, [SECRETS]);
}

#[tokio::test]
async fn a_missing_expected_topic_is_caught_by_coverage_only() {
    let outbox: Vec<_> = clean_outbox()
        .into_iter()
        .filter(|r| r.event.topic.as_str() != "doc_posted")
        .collect();
    assert_eq!(
        failed_on(outbox, vec![], expected_for_engine_world()).await,
        [COVERAGE]
    );
}
