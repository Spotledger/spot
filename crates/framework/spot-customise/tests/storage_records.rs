#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! The records in the three platform DocTypes over a storage backend: one transaction per
//! commit, stable ids, ordering, and the whole service running on top of them.

mod common;
use common::*;
use spot_customise::storage_records::definitions;
use spot_customise::{
    CustomiseService, DocTypeRecord, FieldRecord, Parts, PropertyRecord, RecordKey, RecordOp,
    RecordState, RecordStore, StorageRecordStore,
};
use spot_meta::{FieldSpec, FieldType};
use spot_storage_contract::{Collation, StorageBackend};
use spot_storage_fake::FakeBackend;
use std::sync::Arc;

async fn store() -> (Arc<StorageRecordStore<FakeBackend>>, FakeBackend) {
    let backend = FakeBackend::new();
    let catalog =
        StorageRecordStore::<FakeBackend>::catalog(Collation::new("und").unwrap()).unwrap();
    backend.prepare(&catalog).await.unwrap();
    (
        Arc::new(StorageRecordStore::new(
            Arc::new(backend.clone()),
            Arc::new(catalog),
        )),
        backend,
    )
}

fn field_record(name: &str, intent: u64) -> FieldRecord {
    FieldRecord {
        doctype: "invoice".into(),
        name: name.into(),
        spec: format!("name = \"{name}\"\ntype = \"text\"\n"),
        insert_after: None,
        state: RecordState::Pending,
        reason: None,
        intent,
        actor: "alice".into(),
    }
}

#[test]
fn the_three_platform_doctypes_parse_and_build() {
    let defs = definitions().unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["custom_doctype", "custom_field", "property_setter"]
    );
    let meta = spot_meta::Registry::new(defs).unwrap().effective();
    // Each carries the fields the brief names.
    let field_names = |doctype: &str| -> Vec<String> {
        meta.definition(doctype)
            .unwrap()
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect()
    };
    assert!(field_names("custom_doctype").contains(&"definition".to_owned()));
    assert!(field_names("custom_doctype").contains(&"module".to_owned()));
    assert!(field_names("custom_field").contains(&"insert_after".to_owned()));
    assert!(field_names("custom_field").contains(&"spec".to_owned()));
    for name in ["field", "property", "value"] {
        assert!(
            field_names("property_setter").contains(&name.to_owned()),
            "{name}"
        );
    }
}

#[tokio::test]
async fn records_round_trip_in_write_order_and_a_put_replaces() {
    let (store, _) = store().await;
    store
        .commit(vec![
            RecordOp::PutField(field_record("custom_b", 2)),
            RecordOp::PutField(field_record("custom_a", 1)),
            RecordOp::PutDocType(DocTypeRecord {
                name: "ticket".into(),
                module: "custom".into(),
                definition: "name = \"ticket\"\n".into(),
                state: RecordState::Pending,
                reason: None,
                intent: 3,
                actor: "alice".into(),
                prior: None,
            }),
            RecordOp::PutProperty(PropertyRecord {
                doctype: "invoice".into(),
                field: Some("amount".into()),
                property: "read_only".into(),
                value: "true".into(),
                state: RecordState::Applied,
                reason: Some("earlier".into()),
                intent: 4,
                actor: "sam".into(),
                prior: Some("false".into()),
            }),
        ])
        .await
        .unwrap();
    let loaded = store.load().await.unwrap();
    let order: Vec<&str> = loaded.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(order, vec!["custom_a", "custom_b"], "oldest change first");
    assert_eq!(loaded.doctypes[0].name, "ticket");
    assert_eq!(loaded.properties[0].prior.as_deref(), Some("false"));
    assert_eq!(loaded.properties[0].field.as_deref(), Some("amount"));

    // The same key again is an upsert, not a second row.
    let mut again = field_record("custom_a", 5);
    again.state = RecordState::Applied;
    store.commit(vec![RecordOp::PutField(again)]).await.unwrap();
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.fields.len(), 2);
    assert_eq!(
        loaded.field("invoice", "custom_a").unwrap().state,
        RecordState::Applied
    );

    store
        .commit(vec![RecordOp::Remove(RecordKey::Field(
            "invoice".into(),
            "custom_a".into(),
        ))])
        .await
        .unwrap();
    assert!(
        store
            .load()
            .await
            .unwrap()
            .field("invoice", "custom_a")
            .is_none()
    );
}

#[tokio::test]
async fn a_commit_is_all_or_nothing() {
    let (store, _) = store().await;
    let mut bad = field_record("custom_b", 2);
    bad.doctype = "x".repeat(200); // longer than the DocType's 63 characters: the write is refused
    let err = store
        .commit(vec![
            RecordOp::PutField(field_record("custom_a", 1)),
            RecordOp::PutField(bad),
        ])
        .await
        .unwrap_err();
    assert!(!err.is_empty());
    assert!(
        store.load().await.unwrap().fields.is_empty(),
        "the first put was rolled back with the second"
    );
}

#[tokio::test]
async fn the_service_runs_on_the_storage_records_and_recovers_from_them() {
    let rig = Rig::new().await;
    let (records, _) = store().await;
    let build = |rig: &Rig, records: Arc<StorageRecordStore<FakeBackend>>| {
        CustomiseService::new(Parts {
            site: site(),
            base: base_definitions(),
            options: spot_meta::BuildOptions::new(),
            live: rig.live.clone(),
            bus: rig.bus.clone(),
            versions: rig.versions.clone(),
            definitions: rig.versions.clone(),
            records,
            schema: rig.schema.clone(),
        })
    };
    let service = build(&rig, records.clone());
    service
        .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
        .await
        .unwrap();
    service
        .add_custom_field(
            &admin(),
            "invoice",
            &FieldSpec::new("custom_a", FieldType::Text),
            None,
        )
        .await
        .unwrap();
    let stored = records.load().await.unwrap();
    assert_eq!(stored.doctypes.len(), 2);
    assert!(
        stored
            .doctypes
            .iter()
            .all(|d| d.state == RecordState::Applied)
    );
    assert_eq!(stored.fields[0].state, RecordState::Applied);

    // A restart reads the same records and converges on them.
    let again = Rig::new().await;
    let restarted = build(&again, records);
    let recovery = restarted.recover().await.unwrap();
    assert!(recovery.swapped.is_some());
    let snapshot = again.live.current();
    assert!(snapshot.meta().definition("ticket").is_some());
    assert!(
        snapshot
            .meta()
            .definition("invoice")
            .unwrap()
            .fields
            .iter()
            .any(|f| f.name == "custom_a")
    );
}
