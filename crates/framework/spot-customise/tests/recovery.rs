#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! Crash recovery: a crash between any two pipeline steps converges on `recover()`; a change that
//! cannot apply is rolled back to `failed` with its reason and never blocks the next one.

mod common;
use common::*;
use serde_json::json;
use spot_customise::{CustomiseError, FaultHook, RecordState};
use spot_meta::{FieldSpec, FieldType};
use spot_meta_live::VersionStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// A hook that stops the pipeline once, at `step`.
fn crash_at(step: &'static str) -> Arc<FaultHook> {
    let fired = AtomicBool::new(false);
    Arc::new(move |at| at == step && !fired.swap(true, Ordering::SeqCst))
}

fn has_field(rig: &Rig, doctype: &str, name: &str) -> bool {
    rig.live
        .current()
        .meta()
        .definition(doctype)
        .is_some_and(|d| d.fields.iter().any(|f| f.name == name))
}

#[tokio::test]
async fn a_crash_after_each_step_converges_on_recover() {
    for step in [
        "validated",
        "persisted",
        "schema_applied",
        "backfilled",
        "marked",
        "swapped",
        "published",
    ] {
        let durable = Durable::new().await;
        let rig = Rig::boot(&durable).await;
        rig.service.set_fault_hook(Some(crash_at(step)));
        let err = rig
            .service
            .add_custom_field(&admin(), "invoice", &field("custom_note"), None)
            .await
            .unwrap_err();
        assert_eq!(err, CustomiseError::Interrupted { step }, "{step}");

        // The process died: a new one boots from the base and the shared database.
        let restarted = Rig::boot(&durable).await;
        assert!(
            !has_field(&restarted, "invoice", "custom_note"),
            "{step}: not live yet"
        );
        let recovery = restarted.service.recover().await.unwrap();

        if step == "validated" {
            assert!(
                !recovery.resolved && recovery.swapped.is_none(),
                "{step}: nothing to do"
            );
            assert!(
                durable.records.snapshot().fields.is_empty(),
                "{step}: nothing persisted"
            );
            assert!(!has_field(&restarted, "invoice", "custom_note"));
            continue;
        }
        let records = durable.records.snapshot();
        assert_eq!(records.fields.len(), 1, "{step}");
        assert_eq!(records.fields[0].state, RecordState::Applied, "{step}");
        assert!(
            has_field(&restarted, "invoice", "custom_note"),
            "{step}: the field is live"
        );
        assert!(
            durable
                .schema
                .applied()
                .entity(&spot_storage_contract::Name::new("invoice").unwrap())
                .unwrap()
                .fields
                .iter()
                .any(|f| f.name.as_str() == "custom_note"),
            "{step}: the column exists"
        );
        // The cluster head names exactly what this node serves.
        let head = durable.versions.read().await.unwrap().unwrap();
        assert_eq!(&head.1, restarted.live.current().hash(), "{step}");
        assert_eq!(head.0, restarted.live.current().version(), "{step}");
        // ... and the field is usable through the real router at once.
        let created = restarted
            .call(
                "POST",
                "/api/v1/resource/invoice",
                &[],
                Some(json!({"title":"after crash","custom_note":"ok"})),
            )
            .await;
        created.status(201);
        // A second recover has nothing to do.
        let again = restarted.service.recover().await.unwrap();
        assert!(
            !again.resolved && again.swapped.is_none(),
            "{step}: idempotent"
        );
        // Audit: written once for the change unless the crash came before it.
        let audited = durable
            .host
            .audit()
            .iter()
            .filter(|a| a.action == "add_custom_field")
            .count();
        assert_eq!(audited, 1, "{step}");
    }
}

#[tokio::test]
async fn a_later_change_settles_a_crashed_one_first() {
    let durable = Durable::new().await;
    let rig = Rig::boot(&durable).await;
    rig.service.set_fault_hook(Some(crash_at("persisted")));
    rig.service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap_err();
    let restarted = Rig::boot(&durable).await;
    // No explicit recover: the next change finishes the pending one before it starts.
    restarted
        .service
        .set_property(&admin(), "invoice", None, "label", "Bill")
        .await
        .unwrap();
    assert!(has_field(&restarted, "invoice", "custom_a"));
    assert_eq!(
        restarted
            .live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .label
            .as_deref(),
        Some("Bill")
    );
    assert!(
        durable
            .records
            .snapshot()
            .fields
            .iter()
            .all(|f| f.state == RecordState::Applied)
    );
}

#[tokio::test]
async fn a_schema_failure_rolls_the_records_back_and_does_not_block_the_next_change() {
    let rig = Rig::new().await;
    rig.flaky.fail_apply.store(true, Ordering::SeqCst);
    let version = rig.live.current().version();
    let err = rig
        .service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "customise.schema_failed");
    let records = rig.records.snapshot();
    assert_eq!(records.fields[0].state, RecordState::Failed);
    assert!(
        records.fields[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("not reachable")
    );
    assert_eq!(
        rig.live.current().version(),
        version,
        "the previous snapshot is still serving"
    );
    assert!(!has_field(&rig, "invoice", "custom_a"));

    // The database is back: the next change is not blocked by the failed record, and the same
    // name may be tried again over it.
    rig.flaky.fail_apply.store(false, Ordering::SeqCst);
    rig.service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap();
    assert!(has_field(&rig, "invoice", "custom_a"));
    assert_eq!(rig.records.snapshot().fields[0].state, RecordState::Applied);
}

#[tokio::test]
async fn recovery_rolls_back_a_pending_change_whose_plan_cannot_apply() {
    let durable = Durable::new().await;
    let rig = Rig::boot(&durable).await;
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Bill")
        .await
        .unwrap();
    rig.service.set_fault_hook(Some(crash_at("persisted")));
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Sales Bill")
        .await
        .unwrap_err();

    // Restart on a database whose plan now holds a refused step.
    durable.flaky.refuse.store(true, Ordering::SeqCst);
    let restarted = Rig::boot(&durable).await;
    let recovery = restarted.service.recover().await.unwrap();
    assert!(recovery.resolved);
    let records = durable.records.snapshot();
    let label = records.property("invoice", None, "label").unwrap();
    assert_eq!(label.state, RecordState::Applied);
    assert_eq!(
        label.value, "Bill",
        "a replaced value goes back to its previous one"
    );
    assert!(
        label
            .reason
            .as_deref()
            .unwrap()
            .contains("dropping a field removes data")
    );
    assert_eq!(
        restarted
            .live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .label
            .as_deref(),
        Some("Bill"),
        "the previous state is what serves"
    );

    // A new field that cannot apply ends as `failed`, with the reason.
    durable.flaky.refuse.store(false, Ordering::SeqCst);
    restarted
        .service
        .set_fault_hook(Some(crash_at("persisted")));
    let crashed = restarted
        .service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap_err();
    assert_eq!(crashed.code(), "customise.interrupted", "{crashed}");
    durable.flaky.refuse.store(true, Ordering::SeqCst);
    let again = Rig::boot(&durable).await;
    again.service.recover().await.unwrap();
    let records = durable.records.snapshot();
    assert_eq!(records.fields[0].state, RecordState::Failed, "{records:?}");
    assert!(records.fields[0].reason.is_some());
    assert!(!has_field(&again, "invoice", "custom_a"));

    durable.flaky.refuse.store(false, Ordering::SeqCst);
    again
        .service
        .add_custom_field(&admin(), "invoice", &field("custom_b"), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn two_callers_at_once_both_land() {
    let rig = Rig::new().await;
    let (who, fa, fb) = (admin(), field("custom_a"), field("custom_b"));
    let a = rig.service.add_custom_field(&who, "invoice", &fa, None);
    let b = rig.service.add_custom_field(&who, "invoice", &fb, None);
    let (a, b) = tokio::join!(a, b);
    a.unwrap();
    b.unwrap();
    assert!(has_field(&rig, "invoice", "custom_a") && has_field(&rig, "invoice", "custom_b"));
    let versions = (rig.live.current().version().get(), durable_head(&rig).await);
    assert_eq!(versions.0, versions.1);
}

async fn durable_head(rig: &Rig) -> u64 {
    rig.versions.read().await.unwrap().unwrap().0.get()
}
