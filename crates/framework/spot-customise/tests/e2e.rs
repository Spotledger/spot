#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! Acceptance of WP-PLT-16: create a DocType at runtime, then full CRUD over REST with no
//! restart; a custom field on a DocType holding 20,000 rows with a writer running; overrides in
//! the served metadata; the new type in the default views and the typed declarations.

mod common;
use common::*;
use serde_json::json;
use spot_customise::codes;
use spot_meta::{FieldSpec, FieldType};
use spot_storage_contract::{
    DocumentData, DocumentStore, Isolation, Name, RecordId, StorageBackend, Transaction, TxOptions,
    Value,
};
use spot_views::ViewKind;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn a_new_doctype_is_usable_over_rest_at_once_with_no_restart() {
    let rig = Rig::new().await;
    let party = rig
        .call(
            "POST",
            "/api/v1/resource/party",
            &[],
            Some(json!({"party_name": "Aurora"})),
        )
        .await;
    party.status(201);
    let before = rig.call("GET", "/api/v1/meta/ticket", &[], None).await;
    assert_eq!(before.status, 404, "the type does not exist yet");

    rig.service
        .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
        .await
        .unwrap();

    // The very next request on the same router sees it.
    let meta = rig.call("GET", "/api/v1/meta/ticket", &[], None).await;
    meta.status(200);
    let names: Vec<&str> = meta.body["data"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    for expected in ["subject", "priority", "customer", "notes"] {
        assert!(names.contains(&expected), "{expected} in {names:?}");
    }
    let priority = meta.body["data"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "priority")
        .unwrap();
    assert_eq!(priority["select"], json!(["low", "normal", "urgent"]));

    // create
    let created = rig
        .call(
            "POST",
            "/api/v1/resource/ticket",
            &[],
            Some(json!({
                "subject": "Printer on fire",
                "priority": "urgent",
                "customer": party.id(),
                "notes": [{"text": "smoke"}, {"text": "more smoke"}]
            })),
        )
        .await;
    created.status(201);
    assert_eq!(created.body["data"]["notes"].as_array().unwrap().len(), 2);
    for subject in ["Cable", "Another", "Zebra"] {
        rig.call(
            "POST",
            "/api/v1/resource/ticket",
            &[],
            Some(json!({"subject": subject})),
        )
        .await
        .status(201);
    }

    // read
    let path = format!("/api/v1/resource/ticket/{}", created.id());
    let read = rig.call("GET", &path, &[], None).await;
    read.status(200);
    assert_eq!(read.body["data"]["subject"], "Printer on fire");
    assert_eq!(read.body["data"]["customer"], party.id().as_str());

    // update (If-Match), with a stale write refused
    let patch = rig
        .call(
            "PATCH",
            &path,
            &[("if-match", read.etag())],
            Some(json!({"subject": "Printer fixed", "priority": "low"})),
        )
        .await;
    patch.status(200);
    assert_eq!(patch.body["data"]["subject"], "Printer fixed");
    let stale = rig
        .call(
            "PATCH",
            &path,
            &[("if-match", read.etag())],
            Some(json!({"subject": "Lost"})),
        )
        .await;
    assert_eq!(stale.status, 412);

    // a select value outside the options is refused
    let bad = rig
        .call(
            "POST",
            "/api/v1/resource/ticket",
            &[],
            Some(json!({"subject": "x", "priority": "whenever"})),
        )
        .await;
    assert!(bad.status >= 400 && bad.status < 500, "{}", bad.body);

    // list: filter and sort
    let filtered = rig
        .call(
            "POST",
            "/api/v1/resource/ticket/query",
            &[],
            Some(json!({
                "filter": {"field": "priority", "op": "eq", "value": "normal"},
                "fields": ["subject"],
                "order": [{"field": "subject", "dir": "desc"}]
            })),
        )
        .await;
    filtered.status(200);
    let subjects: Vec<&str> = filtered.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        subjects,
        vec!["Zebra", "Cable", "Another"],
        "filtered and sorted"
    );
    let all = rig.call("GET", "/api/v1/resource/ticket", &[], None).await;
    all.status(200);
    assert_eq!(all.body["data"].as_array().unwrap().len(), 4);

    // delete
    let fresh = rig.call("GET", &path, &[], None).await;
    rig.call("DELETE", &path, &[("if-match", fresh.etag())], None)
        .await
        .status(204);
    rig.call("GET", &path, &[], None).await.status(404);
}

#[tokio::test]
async fn the_new_doctype_appears_in_the_default_views_and_the_typed_declarations() {
    let rig = Rig::new().await;
    rig.service
        .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
        .await
        .unwrap();
    let snapshot = rig.live.current();
    for kind in [ViewKind::Form, ViewKind::List] {
        let view = spot_views::default_view(snapshot.meta(), "ticket", kind).unwrap();
        let text = serde_json::to_string(&view).unwrap();
        for field in ["subject", "priority", "customer"] {
            assert!(
                text.contains(field),
                "{kind:?} view shows `{field}`: {text}"
            );
        }
    }
    let generated = spot_dts::generate(snapshot.meta().definitions()).unwrap();
    assert!(
        generated.text.contains("interface Ticket"),
        "{}",
        generated.text
    );
    assert!(generated.text.contains("subject"));

    // A custom field added later shows up in the same generated artefacts.
    rig.service
        .add_custom_field(
            &admin(),
            "ticket",
            &FieldSpec::new("custom_site", FieldType::Text),
            None,
        )
        .await
        .unwrap();
    let snapshot = rig.live.current();
    let view = spot_views::default_view(snapshot.meta(), "ticket", ViewKind::Form).unwrap();
    assert!(
        serde_json::to_string(&view)
            .unwrap()
            .contains("custom_site")
    );
    let next = spot_dts::generate(snapshot.meta().definitions()).unwrap();
    assert!(next.text.contains("custom_site"));
    assert!(
        spot_dts::changed(&generated.fingerprint, &next),
        "the editor reloads"
    );
}

/// Inserts `count` invoices straight into storage in chunks, as a populated site would have.
async fn seed_invoices(rig: &Rig, count: usize) {
    let catalog = rig.schema.applied();
    let entity = Name::new("invoice").unwrap();
    let def = catalog.entity(&entity).unwrap().clone();
    let mut done = 0;
    while done < count {
        let take = (count - done).min(2_000);
        let options = TxOptions::new(Name::new("seed").unwrap(), Isolation::ReadCommitted);
        let mut tx = rig.backend.begin(catalog.clone(), options).await.unwrap();
        for i in 0..take {
            let mut fields = BTreeMap::new();
            for f in &def.fields {
                let value = match f.name.as_str() {
                    "title" => Value::Text(format!("Invoice {}", done + i)),
                    "amount" => Value::Decimal("10.00".parse().unwrap()),
                    _ => Value::Null,
                };
                fields.insert(f.name.clone(), value);
            }
            tx.insert(&DocumentData {
                entity: entity.clone(),
                id: RecordId::new_v7(),
                fields,
                children: BTreeMap::new(),
            })
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        done += take;
    }
}

#[tokio::test]
async fn a_custom_field_on_20000_rows_with_a_writer_running_and_old_rows_read_the_default() {
    let rig = Arc::new(Rig::new().await);
    seed_invoices(&rig, 20_000).await;

    // A mandatory field with no default cannot fill 20,000 rows.
    let mut mandatory = FieldSpec::new("custom_channel", FieldType::Text);
    mandatory.required = true;
    let refused = rig
        .service
        .add_custom_field(&admin(), "invoice", &mandatory, None)
        .await
        .unwrap_err();
    assert_eq!(
        refused.issue_codes(),
        vec![codes::NEEDS_DEFAULT],
        "{refused}"
    );
    // ... with a default it is allowed.
    mandatory.default = Some("web".into());

    // A concurrent writer keeps creating invoices while the field is added.
    let stop = Arc::new(AtomicBool::new(false));
    let created = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let writer = {
        let (rig, stop, created, failed) =
            (rig.clone(), stop.clone(), created.clone(), failed.clone());
        tokio::spawn(async move {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                let r = rig
                    .call(
                        "POST",
                        "/api/v1/resource/invoice",
                        &[],
                        Some(json!({"title": format!("live {n}")})),
                    )
                    .await;
                if r.status == 201 {
                    created.fetch_add(1, Ordering::SeqCst);
                } else {
                    failed.fetch_add(1, Ordering::SeqCst);
                }
                n += 1;
                tokio::task::yield_now().await;
            }
        })
    };
    while created.load(Ordering::SeqCst) < 20 {
        tokio::task::yield_now().await;
    }
    let before = created.load(Ordering::SeqCst);
    rig.service
        .add_custom_field(&admin(), "invoice", &mandatory, Some("amount"))
        .await
        .unwrap();
    while created.load(Ordering::SeqCst) < before + 20 {
        tokio::task::yield_now().await;
    }
    stop.store(true, Ordering::SeqCst);
    writer.await.unwrap();
    assert_eq!(
        failed.load(Ordering::SeqCst),
        0,
        "no write failed while the field was added"
    );

    // Old rows read the default; the new field is writable at once.
    let listing = rig
        .call(
            "POST",
            "/api/v1/resource/invoice/query",
            &[],
            Some(json!({
                "fields": ["title", "custom_channel"],
                "filter": {"field": "title", "op": "eq", "value": "Invoice 17"}
            })),
        )
        .await;
    listing.status(200);
    let old_id = listing.body["data"][0]["id"].as_str().unwrap().to_owned();
    let old = rig
        .call(
            "GET",
            &format!("/api/v1/resource/invoice/{old_id}"),
            &[],
            None,
        )
        .await;
    println!("GET old: {}", old.body);
    assert_eq!(
        listing.body["data"][0]["custom_channel"], "web",
        "{}",
        listing.body
    );
    let written = rig
        .call(
            "POST",
            "/api/v1/resource/invoice",
            &[],
            Some(json!({"title": "new", "custom_channel": "phone"})),
        )
        .await;
    written.status(201);
    assert_eq!(written.body["data"]["custom_channel"], "phone");
}

#[tokio::test]
async fn a_label_and_a_read_only_override_reach_the_served_metadata() {
    let rig = Rig::new().await;
    let before = rig.call("GET", "/api/v1/meta/invoice", &[], None).await;
    before.status(200);
    assert_eq!(before.body["data"]["label"], "Invoice");
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Sales Invoice")
        .await
        .unwrap();
    rig.service
        .set_property(&admin(), "invoice", Some("amount"), "read_only", "true")
        .await
        .unwrap();
    let after = rig.call("GET", "/api/v1/meta/invoice", &[], None).await;
    after.status(200);
    assert_eq!(after.body["data"]["label"], "Sales Invoice");
    let amount = after.body["data"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "amount")
        .unwrap();
    assert_eq!(amount["read_only"], true);
    let title = after.body["data"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "title")
        .unwrap();
    assert_eq!(title["read_only"], false);
}
