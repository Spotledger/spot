//! Proof (b), (c) and (e): a scaffolded app, the TypeScript boundary, two apps composed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{doctypes_of, required_values, sample_harness, scaffolded};
use serde_json::json;
use spot_app_testkit::{AppHarness, assert};

#[tokio::test]
async fn a_freshly_scaffolded_app_loads_and_its_doctype_round_trips() {
    let (_project, app) = scaffolded("acme");
    let harness = AppHarness::load(&app).await.unwrap();
    let doctype = doctypes_of(&app)
        .into_iter()
        .next()
        .expect("a generated DocType");
    let admin = harness.as_user("Administrator").unwrap();
    let input = required_values(&harness, &doctype);
    let created = admin.create(&doctype, &input).await;
    assert::assert_ok(&created);
    let read = admin.get(&doctype, &created.id().unwrap()).await;
    assert::assert_ok(&read);
    assert::assert_document_eq(created.doc().unwrap(), read.doc().unwrap());
}

#[tokio::test]
async fn two_apps_compose_in_one_harness() {
    let harness = sample_harness().await;
    let (_project, app) = scaffolded("acme");
    harness.load_also(&app).await.unwrap();
    let meta = harness.meta();
    assert!(
        meta.doctype("project").is_some(),
        "the first app is still there"
    );
    let doctype = doctypes_of(&app).into_iter().next().unwrap();
    assert!(meta.doctype(&doctype).is_some(), "the second app joined");
    let admin = harness.as_user("Administrator").unwrap();
    let created = admin
        .create(&doctype, &required_values(&harness, &doctype))
        .await;
    assert::assert_ok(&created);
}

#[tokio::test]
async fn the_parts_an_app_does_not_have_answer_plainly() {
    // WP-EXT-07.T3: functions, lists and fixtures are run now; an app that declares none of them
    // gets a plain answer, not an "unsupported" error.
    let harness = sample_harness().await;
    let admin = harness.as_user("Administrator").unwrap();
    let function = admin
        .call_function("sample", "reports", "total", json!({}))
        .await;
    assert::assert_error(&function, "function.not_found");
    let list = admin.list("project", &json!({})).await.unwrap();
    assert!(list.is_empty());
    assert_eq!(harness.load_fixtures().await.unwrap(), 0);
}
