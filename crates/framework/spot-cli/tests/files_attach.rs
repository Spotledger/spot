//! The attach lifecycle over real HTTP and PostgreSQL (WP-PRD-01.T25): a document saved with an
//! `attach` value marks the file attached, a replaced or cleared value and a deleted document
//! orphan it, a file the writer may not attach is not attached, and a step lost to a crash is
//! repaired at the next start.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use serde_json::{Value as Json, json};
use spot_app_api::DocumentPatch;
use spot_app_api::context::{Actor, DocumentService as _, Invocation};
use spot_app_api::prelude::{Name, Value};
use std::net::SocketAddr;

const OTHER_PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";

async fn upload(addr: SocketAddr, host: &str, token: &str, body: &[u8]) -> String {
    let reply = http_raw(
        addr,
        host,
        Some(token),
        "POST",
        "/api/v1/files",
        &[("Content-Disposition", "attachment; filename=\"data.bin\"")],
        ("application/octet-stream", body),
    )
    .await;
    expect(&reply, 201);
    reply.body["data"]["id"].as_str().unwrap().to_owned()
}

/// The lifecycle state and the document a file hangs off, read as the administrator.
async fn state_of(addr: SocketAddr, host: &str, admin: &str, file: &str) -> (String, Json) {
    let reply = http(
        addr,
        host,
        Some(admin),
        "GET",
        &format!("/api/v1/resource/file/{file}"),
        &[],
        None,
    )
    .await;
    expect(&reply, 200);
    let data = &reply.body["data"];
    (
        data["state"].as_str().unwrap().to_owned(),
        data["attached_id"].clone(),
    )
}

async fn download(addr: SocketAddr, host: &str, token: &str, file: &str) -> Reply {
    http(
        addr,
        host,
        Some(token),
        "GET",
        &format!("/api/v1/files/{file}"),
        &[],
        None,
    )
    .await
}

async fn new_project(
    addr: SocketAddr,
    host: &str,
    admin: &str,
    owner: &str,
    attachment: Json,
) -> Reply {
    http(
        addr,
        host,
        Some(admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": format!("PRJ-A-{}", uuid_tail(&attachment)), "title": "Files", "status": "Active",
            "owner": owner, "currency": "USD", "budget": "10.00",
            "attachments": attachment
        })),
    )
    .await
}

fn uuid_tail(value: &Json) -> String {
    value
        .as_str()
        .map_or_else(|| "none".to_owned(), |s| s[s.len() - 6..].to_owned())
}

async fn patch(
    addr: SocketAddr,
    host: &str,
    admin: &str,
    project: &Reply,
    attachment: Json,
) -> Reply {
    let id = project.body["data"]["id"].as_str().unwrap();
    http(
        addr,
        host,
        Some(admin),
        "PATCH",
        &format!("/api/v1/resource/project/{id}"),
        &[("If-Match", &project.etag)],
        Some(json!({ "attachments": attachment })),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_value_attaches_a_replaced_or_cleared_one_orphans_and_a_delete_orphans() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    env.user("other@example.com", OTHER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let other = token_of(addr, &host, "other@example.com", OTHER_PASSWORD).await;
    let other_id = user_id(addr, &host, &admin, "other@example.com").await;

    let a = upload(addr, &host, &admin, b"first file").await;
    let b = upload(addr, &host, &admin, b"second file").await;
    assert_eq!(state_of(addr, &host, &admin, &a).await.0, "pending");

    // Saved naming the first file: it is attached, the administrator (a reader of the document)
    // downloads it, and a user who may not read the document learns nothing: 404.
    let created = new_project(addr, &host, &admin, &other_id, json!(a)).await;
    expect(&created, 201);
    let project_id = created.body["data"]["id"].as_str().unwrap().to_owned();
    let (state, target) = state_of(addr, &host, &admin, &a).await;
    assert_eq!(state, "attached");
    assert_eq!(target, json!(project_id));
    let read = download(addr, &host, &admin, &a).await;
    expect(&read, 200);
    assert_eq!(read.bytes, b"first file");
    expect(&download(addr, &host, &other, &a).await, 404);

    // Replaced: the old file is orphaned, the new one attached.
    let replaced = patch(addr, &host, &admin, &created, json!(b)).await;
    expect(&replaced, 200);
    assert_eq!(state_of(addr, &host, &admin, &a).await.0, "orphaned");
    let (state, target) = state_of(addr, &host, &admin, &b).await;
    assert_eq!(state, "attached");
    assert_eq!(target, json!(project_id));

    // Cleared: the same.
    let cleared = patch(addr, &host, &admin, &replaced, Json::Null).await;
    expect(&cleared, 200);
    assert_eq!(state_of(addr, &host, &admin, &b).await.0, "orphaned");

    // Deleted: the files that were attached to it are orphaned.
    let c = upload(addr, &host, &admin, b"third file").await;
    let attached = patch(addr, &host, &admin, &cleared, json!(c)).await;
    expect(&attached, 200);
    assert_eq!(state_of(addr, &host, &admin, &c).await.0, "attached");
    let deleted = http(
        addr,
        &host,
        Some(&admin),
        "DELETE",
        &format!("/api/v1/resource/project/{project_id}"),
        &[("If-Match", &attached.etag)],
        None,
    )
    .await;
    expect(&deleted, 204);
    assert_eq!(state_of(addr, &host, &admin, &c).await.0, "orphaned");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_save_naming_a_file_its_writer_may_not_attach_attaches_nothing() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    env.user("other@example.com", OTHER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let other = token_of(addr, &host, "other@example.com", OTHER_PASSWORD).await;
    let other_id = user_id(addr, &host, &admin, "other@example.com").await;

    // Somebody else's unattached file: the save may be refused or may go through, but the file
    // is not attached to the document and stays the uploader's alone.
    let theirs = upload(addr, &host, &other, b"private to other").await;
    let reply = new_project(addr, &host, &admin, &other_id, json!(theirs)).await;
    assert!(
        reply.status == 201 || (400..500).contains(&reply.status),
        "{}",
        reply.status
    );
    assert_eq!(state_of(addr, &host, &admin, &theirs).await.0, "pending");
    expect(&download(addr, &host, &admin, &theirs).await, 404);
    expect(&download(addr, &host, &other, &theirs).await, 200);

    // An id that names no file: the save may be refused or accepted (the engine checks the link
    // only as far as the writer can see), but there is nothing to attach and nothing breaks.
    let unknown = new_project(
        addr,
        &host,
        &admin,
        &other_id,
        json!("0192f3c4-0000-7000-8000-00000000c0aa"),
    )
    .await;
    assert!(
        unknown.status == 201 || (400..500).contains(&unknown.status),
        "{}",
        unknown.status
    );
    if unknown.status == 201 {
        // The step ran and found nothing: the document keeps the dangling value, no file moved.
        assert_eq!(state_of(addr, &host, &admin, &theirs).await.0, "pending");
    }
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_lost_between_commit_and_delivery_is_repaired_at_the_next_start() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    env.user("other@example.com", OTHER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let other_id = user_id(addr, &host, &admin, "other@example.com").await;

    let file = upload(addr, &host, &admin, b"survives a crash").await;
    let created = new_project(addr, &host, &admin, &other_id, json!(file)).await;
    expect(&created, 201);
    assert_eq!(state_of(addr, &host, &admin, &file).await.0, "attached");

    // The crash: the document committed, the attach step never ran. Put the record back as the
    // upload left it.
    let live = started.running.live(&host).expect("the site is serving");
    let engine = live.engine().expect("the engine is running");
    let inv = Invocation {
        site: spot_core::SiteId::new(&host).unwrap(),
        actor: Actor::System {
            component: Name::new("test").unwrap(),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::now_utc(),
    };
    let id = spot_http::api::codec::parse_record_id(&file).unwrap();
    let loaded = engine.load(&inv, "file", id).await.unwrap();
    let mut patch = DocumentPatch::default();
    for (field, value) in [
        ("state", Value::Text("pending".to_owned())),
        ("attached_doctype", Value::Null),
        ("attached_id", Value::Null),
        ("attached_field", Value::Null),
    ] {
        patch.fields.insert(Name::new(field).unwrap(), value);
    }
    engine
        .update(&inv, "file", id, patch, loaded.value.version().unwrap())
        .await
        .unwrap();
    assert_eq!(state_of(addr, &host, &admin, &file).await.0, "pending");
    started.finish().await;

    // The next start re-applies the step for the document that names the file.
    let again = start(&env).await;
    let admin = token_of(again.addr, &host, "administrator", ADMIN_PASSWORD).await;
    let (state, target) = state_of(again.addr, &host, &admin, &file).await;
    assert_eq!(state, "attached");
    assert_eq!(target, created.body["data"]["id"]);
    again.finish().await;
}
