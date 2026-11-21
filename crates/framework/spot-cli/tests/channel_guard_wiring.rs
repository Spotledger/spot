//! The SCIM and print channels through the field guard on a running `spot serve`
//! (WP-SEC-04.T16), over real HTTP and the test PostgreSQL (skipped, with no output, when
//! `SPOT_TEST_PG_URL` is unset).
//!
//! WP-SEC-04.T10 and T12 built the guarded entry points; this suite proves the host calls them.
//! Each test plants a canary, proves an entitled caller sees it (the positive control, so an
//! absent canary is not an empty table or a template that never prints it), and then proves the
//! caller the guard hides it from never gets it: not in the body, not in a filter's answer.
//!
//! There is no unguarded form of the host to run the same requests against: the SCIM service
//! runs the core only through `handle_guarded`, and `PrintService::render_document` is private to
//! `services/print.rs` and called only after `guard_document`, so the old path is gone by type.
//! SCIM groups have no DocType and stay unguarded (see `services/scim.rs`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::*;
use serde_json::{Value as Json, json};
use spot_cli::services::scim::{ScimKeyIssuer, ScimService};
use spot_cli::services::secrets as svc;
use spot_core::SystemClock;
use spot_iam_scim::{Limits, parse_record_id};

const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const PLANTED: &str = "PLANTED-CONFIDENTIAL-7731";
const CANARY_USER: &str = "canary-person-4c9e1a@example.com";

fn wire(reply: &Reply) -> String {
    String::from_utf8_lossy(&reply.bytes).into_owned()
}

// ------------------------------------------------------------------------------------- SCIM

struct ScimRig {
    env: Env,
    dir: PathBuf,
    started: Option<Started>,
    admin: String,
    bot: String,
    key: String,
}

impl Drop for ScimRig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl ScimRig {
    /// A host with a vault and SCIM on, a user carrying the canary, a `service` user that owns a
    /// SCIM key and holds **no role**. `None` when no PostgreSQL is configured.
    async fn new() -> Option<ScimRig> {
        let dir = std::env::temp_dir().join(format!(
            "spot_scim_t16_{}",
            spot_app_api::prelude::RecordId::new_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let services = format!(
            "[services.secrets]\nkek_file = {:?}\n[services.scim]\nenabled = true\nrequests_per_window = 100000\n",
            format!("{}/{{site}}.json", dir.display())
        );
        let env = Env::new(&services).await?;
        let first = env.site();
        svc::init_key_file(&dir.join(format!("{}.json", env.site)), &env.site)
            .await
            .unwrap();
        env.user(CANARY_USER, ADMIN_PASSWORD);
        let started = start(&env).await;
        let admin = admin(started.addr, &env.site, &first).await;
        let mut rig = ScimRig {
            env,
            dir,
            started: Some(started),
            admin,
            bot: String::new(),
            key: String::new(),
        };
        rig.bot = rig.service_user().await;
        rig.key = rig
            .issuer()
            .await
            .issue(parse_record_id(&rig.bot).unwrap(), "idp", None)
            .await
            .expect("the key is issued");
        Some(rig)
    }

    fn started(&self) -> &Started {
        self.started.as_ref().unwrap()
    }

    async fn native(&self, method: &str, path: &str, body: Option<Json>) -> Reply {
        http(
            self.started().addr,
            &self.env.site,
            Some(&self.admin),
            method,
            path,
            &[],
            body,
        )
        .await
    }

    async fn scim(&self, path: &str) -> Reply {
        http(
            self.started().addr,
            &self.env.site,
            Some(&self.key),
            "GET",
            path,
            &[],
            None,
        )
        .await
    }

    async fn service_user(&self) -> String {
        let reply = self
            .native(
                "POST",
                "/api/v1/resource/user",
                Some(json!({
                    "user_name": "scim-bot",
                    "full_name": "SCIM bot",
                    "user_type": "service",
                    "state": "active",
                })),
            )
            .await;
        expect(&reply, 201);
        reply.body["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn issuer(&self) -> ScimService {
        let root = &self.env.root.0;
        let loaded = spot_cli::config::load(root, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        let host = svc::open(root, &loaded, site).await.unwrap();
        let meta = spot_cli::services::metadata::effective(root, &loaded, site).unwrap();
        let catalog = Arc::new(spot_cli::schema::catalog(&meta).unwrap());
        let live = self.started().running.live(&self.env.site).unwrap();
        ScimService::new(
            self.env.site.clone(),
            live,
            Arc::new(SystemClock::new()),
            catalog,
            host,
            Limits::default(),
        )
    }

    async fn finish(mut self) {
        if let Some(started) = self.started.take() {
            started.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scim_never_carries_an_attribute_the_client_may_not_read() {
    let Some(rig) = ScimRig::new().await else {
        return;
    };
    let by_name = "/scim/v2/Users?filter=userName%20eq%20%22canary-person-4c9e1a@example.com%22";

    // The SCIM client's owner holds no role: it may read nothing of a user, so no attribute
    // reaches the wire, on a list, on a read of one, or through a filter used as an oracle.
    let list = rig.scim("/scim/v2/Users").await;
    expect(&list, 200);
    assert!(
        !wire(&list).contains(CANARY_USER),
        "a user attribute leaked to a client with no read right: {}",
        wire(&list)
    );
    let filtered = rig.scim(by_name).await;
    assert!(
        !wire(&filtered).contains(CANARY_USER),
        "a filter answered with a value: {}",
        wire(&filtered)
    );
    assert_ne!(
        filtered.body["totalResults"],
        json!(1),
        "a filter on a hidden attribute matched: {}",
        wire(&filtered)
    );

    // Give the owner the right to read users: the same requests now carry the canary.
    // A system role the platform seeds: found, not made.
    let listed = rig
        .native("GET", "/api/v1/resource/role?limit=200", None)
        .await;
    expect(&listed, 200);
    let role = listed.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["role_name"] == "User Manager")
        .and_then(|row| row["id"].as_str())
        .expect("the platform seeds a User Manager role")
        .to_owned();
    assign(
        rig.started().addr,
        &rig.env.site,
        &rig.admin,
        &rig.bot,
        &role,
    )
    .await;
    let entitled = rig.scim("/scim/v2/Users").await;
    expect(&entitled, 200);
    assert!(
        wire(&entitled).contains(CANARY_USER),
        "the positive control: an entitled client sees the user: {}",
        wire(&entitled)
    );
    let matched = rig.scim(by_name).await;
    expect(&matched, 200);
    assert_eq!(matched.body["totalResults"], json!(1), "{}", wire(&matched));
    rig.finish().await;
}

// ------------------------------------------------------------------------------------- print

struct PrintWorld {
    env: Env,
    started: Started,
    admin: String,
    viewer: String,
    task: String,
}

/// Whether `pdf` carries `text` where Typst puts a document title: a literal string, or UTF-16BE
/// in hex, in the Info dictionary (the technique `files_print.rs` uses).
fn pdf_has_text(pdf: &[u8], text: &str) -> bool {
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

/// A host whose Task default format puts every printed field value into the document title, a
/// Viewer who may print a task but not read `details` (group `confidential`), an Administrator
/// who may, and one task carrying the canary in `details`.
async fn print_world() -> Option<PrintWorld> {
    let env = Env::new("").await?;
    // The site first: `new-site` refuses a directory that exists without a site file.
    let first = env.site();
    let templates = env.site_dir().join("templates");
    std::fs::create_dir_all(&templates).unwrap();
    // `fields` holds one entry per field the caller's document still has: a hidden field is not
    // in it, so the title is the whole of what the page shows.
    std::fs::write(
        templates.join("task.typ"),
        "#set document(title: [{% for f in fields %}{{ f.value }} {% endfor %}])\n#set page(paper: \"a4\")\n{{ title }}\n",
    )
    .unwrap();
    env.user("viewer@example.com", VIEWER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;
    let owner = me(addr, &host, &admin).await;
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-CG-1", "title": "Channel guard", "status": "Active",
            "owner": owner, "currency": "USD", "budget": "10.00"
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
        Some(json!({
            "subject": "Guarded printable task",
            "project": project.body["data"]["id"],
            "details": PLANTED
        })),
    )
    .await;
    expect(&task, 201);
    let task = task.body["data"]["id"].as_str().unwrap().to_owned();
    let viewer = token_of(addr, &host, "viewer@example.com", VIEWER_PASSWORD).await;
    Some(PrintWorld {
        env,
        started,
        admin,
        viewer,
        task,
    })
}

impl PrintWorld {
    async fn print(&self, token: &str) -> Reply {
        http(
            self.started.addr,
            &self.env.site,
            Some(token),
            "GET",
            &format!("/api/v1/print/task/{}", self.task),
            &[],
            None,
        )
        .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_printed_page_never_carries_a_field_the_caller_may_not_read() {
    let Some(w) = print_world().await else { return };

    // The entitled caller's page carries the value: the template prints it and the check finds it.
    let admin = w.print(&w.admin).await;
    expect(&admin, 200);
    assert!(admin.bytes.starts_with(b"%PDF-"));
    assert!(
        pdf_has_text(&admin.bytes, PLANTED),
        "the positive control: the page of an entitled caller carries the value"
    );

    // The Viewer prints the same task: the page is made (and carries the plain subject), the
    // protected value is not in it.
    let viewer = w.print(&w.viewer).await;
    expect(&viewer, 200);
    assert!(viewer.bytes.starts_with(b"%PDF-"));
    assert!(
        pdf_has_text(&viewer.bytes, "Guarded printable task"),
        "the viewer's page carries the plain subject"
    );
    assert!(
        !pdf_has_text(&viewer.bytes, PLANTED),
        "a protected value reached a printed page"
    );
    assert!(
        !String::from_utf8_lossy(&viewer.bytes).contains(PLANTED),
        "a protected value is in the bytes of a printed page"
    );
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_platform_print_goes_through_the_guard_too() {
    let Some(w) = print_world().await else { return };
    let live = w
        .started
        .running
        .live(&w.env.site)
        .expect("the site is live");
    let printing = live.print().expect("the site prints");
    let record = spot_http::api::codec::parse_record_id(&w.task).unwrap();
    // The platform's own print is made (the guard can judge a system principal) and is a PDF.
    let pdf = printing
        .render_as_platform("task", record, None)
        .await
        .expect("the platform prints through the guard");
    assert!(pdf.starts_with(b"%PDF-"));
    w.started.finish().await;
}
