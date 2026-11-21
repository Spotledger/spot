//! Files and printing over real HTTP and PostgreSQL: per-file permission (404, never 403),
//! ranges, records that survive a restart, and PDFs made as the caller.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use serde_json::json;
use spot_cli::audit::{CapturingAudit, IdentityAudit};
use std::sync::Arc;

const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const OTHER_PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";

/// A host started with a capturing identity audit and an admin API key; returns the key.
async fn audited_world() -> Option<(Env, Started, Arc<CapturingAudit>, String)> {
    let env = Env::new("").await?;
    let key_file = env.root.0.join("admin.key");
    ok(
        &env.root.0,
        &[
            "new-site",
            &env.site,
            "--admin-api-key-file",
            key_file.to_str().unwrap(),
        ],
    );
    let key = std::fs::read_to_string(&key_file)
        .unwrap()
        .trim()
        .to_owned();
    // Until a password is chosen the key is gated (restricted); lift the gate so a valid key works.
    env.set_password(&env.site, "administrator", VIEWER_PASSWORD);
    let sink = Arc::new(CapturingAudit::new());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let audit: Arc<dyn IdentityAudit> = sink.clone();
    let running =
        spot_cli::start_with_audit(&env.root.0, false, fast_options(), Some(audit), async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    let addr = running.local_addrs()[0];
    Some((
        env,
        Started {
            running,
            addr,
            stop,
        },
        sink,
        key,
    ))
}

/// The right shape, the right id, the wrong secret.
fn wrong_secret(key: &str) -> String {
    let (id, secret) = key.strip_prefix("spk_").unwrap().split_once('_').unwrap();
    format!("spk_{id}_{}", "A".repeat(secret.len()))
}

fn bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn upload(addr: std::net::SocketAddr, host: &str, token: &str, body: &[u8]) -> Reply {
    http_raw(
        addr,
        host,
        Some(token),
        "POST",
        "/api/v1/files",
        &[("Content-Disposition", "attachment; filename=\"data.bin\"")],
        ("application/octet-stream", body),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_is_read_with_a_range_by_its_owner_and_is_a_404_for_everyone_else() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    env.user("other@example.com", OTHER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let other = token_of(addr, &host, "other@example.com", OTHER_PASSWORD).await;

    let content = bytes(1000);
    let stored = upload(addr, &host, &admin, &content).await;
    expect(&stored, 201);
    let id = stored.body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(stored.body["data"]["size"], 1000);
    let path = format!("/api/v1/files/{id}");

    // The owner downloads the whole file and a range of it.
    let whole = http(addr, &host, Some(&admin), "GET", &path, &[], None).await;
    expect(&whole, 200);
    assert_eq!(whole.bytes, content);
    let part = http(
        addr,
        &host,
        Some(&admin),
        "GET",
        &path,
        &[("Range", "bytes=10-19")],
        None,
    )
    .await;
    expect(&part, 206);
    assert_eq!(part.bytes, content[10..20]);
    assert_eq!(part.header("content-range"), Some("bytes 10-19/1000"));

    // Another signed-in user, and a stranger, learn nothing: not found, never forbidden.
    let denied = http(addr, &host, Some(&other), "GET", &path, &[], None).await;
    expect(&denied, 404);
    assert_eq!(denied.body["error"]["code"], "document.not_found");
    let unknown = http(
        addr,
        &host,
        Some(&other),
        "GET",
        "/api/v1/files/0192f3c4-0000-7000-8000-00000000c009",
        &[],
        None,
    )
    .await;
    expect(&unknown, 404);
    assert_eq!(unknown.body["error"]["code"], denied.body["error"]["code"]);
    assert_eq!(
        unknown.body["error"]["message"],
        denied.body["error"]["message"]
    );
    expect(&http(addr, &host, None, "GET", &path, &[], None).await, 401);

    // The record survives a restart of the host.
    started.finish().await;
    let again = start(&env).await;
    let admin = token_of(again.addr, &host, "administrator", ADMIN_PASSWORD).await;
    let reread = http(again.addr, &host, Some(&admin), "GET", &path, &[], None).await;
    expect(&reread, 200);
    assert_eq!(reread.bytes, content);
    let still_denied = http(
        again.addr,
        &host,
        Some(&token_of(again.addr, &host, "other@example.com", OTHER_PASSWORD).await),
        "GET",
        &path,
        &[],
        None,
    )
    .await;
    expect(&still_denied, 404);
    again.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attached_file_is_still_read_with_a_range_by_its_owner_and_a_404_for_another() {
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

    let content = bytes(1000);
    let stored = upload(addr, &host, &admin, &content).await;
    expect(&stored, 201);
    let id = stored.body["data"]["id"].as_str().unwrap().to_owned();
    let path = format!("/api/v1/files/{id}");

    // A document the file hangs off: only the administrator may read it.
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-F-1", "title": "Files", "status": "Active",
            "owner": other_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project_id = project.body["data"]["id"].as_str().unwrap().to_owned();

    // Attach it. The flag that says who may read flips; the bytes stay where they were put.
    let live = started.running.live(&host).expect("the site is serving");
    let files = live.files().expect("the file service is built");
    files
        .attach_saved(
            &spot_core::SiteId::new(&host).unwrap(),
            "project",
            spot_http::api::codec::parse_record_id(&project_id).unwrap(),
            &[(
                "attachments".to_owned(),
                spot_http::api::codec::parse_record_id(&id).unwrap(),
            )],
        )
        .await
        .unwrap();

    // The owner still reads all of it and a range of it, right after the attach and after a
    // restart (the key is recomputed from the id and the creation time, never stored).
    let whole = http(addr, &host, Some(&admin), "GET", &path, &[], None).await;
    expect(&whole, 200);
    assert_eq!(whole.bytes, content);
    let part = http(
        addr,
        &host,
        Some(&admin),
        "GET",
        &path,
        &[("Range", "bytes=100-199")],
        None,
    )
    .await;
    expect(&part, 206);
    assert_eq!(part.bytes, content[100..200]);
    assert_eq!(part.header("content-range"), Some("bytes 100-199/1000"));
    let denied = http(addr, &host, Some(&other), "GET", &path, &[], None).await;
    expect(&denied, 404);
    assert_eq!(denied.body["error"]["code"], "document.not_found");

    started.finish().await;
    let again = start(&env).await;
    let admin = token_of(again.addr, &host, "administrator", ADMIN_PASSWORD).await;
    let part = http(
        again.addr,
        &host,
        Some(&admin),
        "GET",
        &path,
        &[("Range", "bytes=0-9")],
        None,
    )
    .await;
    expect(&part, 206);
    assert_eq!(part.bytes, content[..10]);
    again.finish().await;
}

fn pdf_has_text(pdf: &[u8], text: &str) -> bool {
    // Typst writes the document title into the Info dictionary as a text string: literal, or
    // UTF-16BE in hex.
    let literal = text.as_bytes();
    let mut hex = String::from("FEFF");
    for unit in text.encode_utf16() {
        hex.push_str(&format!("{unit:04X}"));
    }
    pdf.windows(literal.len()).any(|w| w == literal)
        || String::from_utf8_lossy(pdf)
            .to_ascii_uppercase()
            .contains(&hex)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pdf_is_made_as_the_caller_and_is_a_404_for_one_who_may_not_read() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    env.user("viewer@example.com", VIEWER_PASSWORD);
    env.user("other@example.com", OTHER_PASSWORD);
    // The site's own default format for Task: the subject becomes the document title.
    let templates = env.site_dir().join("templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("task.typ"),
        "#set document(title: [{{ doc.subject }}])\n#set page(paper: \"a4\")\n{{ doc.subject }}\n",
    )
    .unwrap();
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;
    let me = me(addr, &host, &admin).await;
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-PR-1", "title": "Print", "status": "Active",
            "owner": me, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let task = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Printable task 777", "project": project.body["data"]["id"]})),
    )
    .await;
    expect(&task, 201);
    let id = task.body["data"]["id"].as_str().unwrap().to_owned();
    let path = format!("/api/v1/print/task/{id}");

    let viewer = token_of(addr, &host, "viewer@example.com", VIEWER_PASSWORD).await;
    let other = token_of(addr, &host, "other@example.com", OTHER_PASSWORD).await;
    let page = http(addr, &host, Some(&viewer), "GET", &path, &[], None).await;
    expect(&page, 200);
    assert_eq!(page.header("content-type"), Some("application/pdf"));
    assert!(page.bytes.starts_with(b"%PDF-"));
    assert!(
        pdf_has_text(&page.bytes, "Printable task 777"),
        "the page does not carry the document's value"
    );
    // A user who may not read the task gets the same 404 as for a task that is not there.
    let denied = http(addr, &host, Some(&other), "GET", &path, &[], None).await;
    expect(&denied, 404);
    let missing = http(
        addr,
        &host,
        Some(&other),
        "GET",
        "/api/v1/print/task/0192f3c4-0000-7000-8000-00000000c009",
        &[],
        None,
    )
    .await;
    expect(&missing, 404);
    assert_eq!(denied.body["error"]["code"], missing.body["error"]["code"]);
    // A format the DocType does not define is its own error, once the read was allowed.
    let unknown = http(
        addr,
        &host,
        Some(&viewer),
        "GET",
        &format!("{path}?format=nope"),
        &[],
        None,
    )
    .await;
    assert_eq!(
        unknown.body["error"]["code"], "print.format_unknown",
        "{}",
        unknown.status
    );
    // Without a site template the built-in layout prints the same document.
    let other_type = http(
        addr,
        &host,
        Some(&admin),
        "GET",
        &format!(
            "/api/v1/print/project/{}",
            project.body["data"]["id"].as_str().unwrap()
        ),
        &[],
        None,
    )
    .await;
    expect(&other_type, 200);
    assert!(pdf_has_text(&other_type.bytes, "PRJ-PR-1"));

    // Job and report code print through the same service, as the platform.
    let printing = started
        .running
        .live(&host)
        .and_then(|live| live.print())
        .expect("the site prints");
    let record = spot_http::api::codec::parse_record_id(&id).unwrap();
    let pdf = printing
        .render_as_platform("task", record, None)
        .await
        .unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    started.finish().await;
}

/// WP-PRD-01.T46: a refused API key on a files route and on a print route is audited once, as
/// an `api` route, and the caller's refusal is the one a refused non-key credential gets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_api_key_on_files_and_print_is_audited_as_an_api_route() {
    let Some((env, started, sink, key)) = audited_world().await else {
        return;
    };
    let (addr, host) = (started.addr, env.site.clone());
    let wrong = wrong_secret(&key);
    let id = "0192f3c4-0000-7000-8000-00000000c009";
    let file = format!("/api/v1/files/{id}");
    let print = format!("/api/v1/print/task/{id}");

    // A valid key: no record.
    let stored = upload(addr, &host, &key, &bytes(10)).await;
    expect(&stored, 201);
    let valid_print = http(addr, &host, Some(&key), "GET", &print, &[], None).await;
    expect(&valid_print, 404);
    assert!(
        sink.of("api_key_rejected").is_empty(),
        "{:?}",
        sink.records()
    );

    // A wrong secret for a real id and a key with an unknown id: the same refusal on each route
    // (no key-id oracle; the request id beside the error differs per request), each audited.
    let (real_id, secret) = key.strip_prefix("spk_").unwrap().split_once('_').unwrap();
    let unknown = format!("spk_{}_{secret}", "a".repeat(real_id.len()));
    for (n, route) in [&file, &print].into_iter().enumerate() {
        let wrong_reply = http(addr, &host, Some(&wrong), "GET", route, &[], None).await;
        let unknown_reply = http(addr, &host, Some(&unknown), "GET", route, &[], None).await;
        assert_eq!(wrong_reply.status, unknown_reply.status, "{route}");
        assert_eq!(
            wrong_reply.body["error"]["code"],
            unknown_reply.body["error"]["code"]
        );
        assert_eq!(
            wrong_reply.body["error"]["message"],
            unknown_reply.body["error"]["message"]
        );
        let rejected = sink.of("api_key_rejected");
        assert_eq!(rejected.len(), 2 * (n + 1), "{:?}", sink.records());
        assert!(rejected.iter().all(|r| r.path.as_deref() == Some("api")));
    }

    // No key material in any record.
    let text = serde_json::to_string(&sink.records()).unwrap();
    for needle in [wrong.as_str(), unknown.as_str(), key.as_str(), "/api/v1"] {
        assert!(!text.contains(needle), "{needle} leaked: {text}");
    }
    started.finish().await;
}
