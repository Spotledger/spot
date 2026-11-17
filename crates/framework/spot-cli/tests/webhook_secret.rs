//! WP-INT-01.T5: a webhook's signing secret has a write-only path and a read path for the
//! delivery handler. Real PostgreSQL and a real `spot serve` (skipped, with no output, when
//! `SPOT_TEST_PG_URL` is unset).
//!
//! What is proved: a secret is set, rotated and described (fingerprint and key version) over the
//! routes and is never in an answer, a document or the database in the clear; a caller without
//! the right is refused the same way whether or not the webhook exists and changes nothing; the
//! delivery handler's port opens the current value, and answers an error (never an empty key)
//! for an unset secret, an unknown webhook, another site and a site with no vault; the engine's
//! audit journal names the caller `webhooks` and holds no value.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Env, Started};
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{Name, RecordId};
use spot_cli::services::secrets as svc;
use spot_cli::services::webhook_secret::{
    SigningKeyError, WebhookSecretService, WebhookSigningKeys,
};
use spot_core::{SiteId, SystemClock};
use spot_document::secrets::{SecretCaller, SecretOp};

const SUPPLIED: &str = "whsec_cli_canary_41d07b9c2e5a8f36";
const ROTATED: &str = "whsec_cli_canary_9a6e3c1f0d7b5284";
const PLAIN_USER: &str = "plain@example.com";

struct Rig {
    env: Env,
    dir: PathBuf,
    started: Option<Started>,
    admin: String,
    plain: String,
    vault: bool,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// A host over PostgreSQL, with a vault (a key file for the site) or without one.
    async fn new(vault: bool) -> Option<Rig> {
        let dir =
            std::env::temp_dir().join(format!("spot_webhook_secret_t5_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let services = if vault {
            format!(
                "[services.secrets]\nkek_file = {:?}\n",
                format!("{}/{{site}}.json", dir.display())
            )
        } else {
            String::new()
        };
        let env = Env::new(&services).await?;
        let first = env.site();
        if vault {
            let kek = dir.join(format!("{}.json", env.site));
            svc::init_key_file(&kek, &env.site).await.unwrap();
        }
        env.user(PLAIN_USER, common::ADMIN_PASSWORD);
        let started = common::start(&env).await;
        let admin = common::admin(started.addr, &env.site, &first).await;
        let plain =
            common::token_of(started.addr, &env.site, PLAIN_USER, common::ADMIN_PASSWORD).await;
        Some(Rig {
            env,
            dir,
            started: Some(started),
            admin,
            plain,
            vault,
        })
    }

    fn started(&self) -> &Started {
        self.started.as_ref().unwrap()
    }

    async fn call(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: Option<Json>,
    ) -> common::Reply {
        common::http(
            self.started().addr,
            &self.env.site,
            token,
            method,
            path,
            &[],
            body,
        )
        .await
    }

    /// Creates a webhook as the Administrator and returns its id.
    async fn webhook(&self, name: &str) -> String {
        let reply = self
            .call(
                Some(&self.admin),
                "POST",
                "/api/v1/resource/webhook",
                Some(json!({
                    "name": name,
                    "url": "https://hooks.example.test/in",
                    "doctype": "project",
                    "events": ["create"],
                    "payload_mode": "ids",
                })),
            )
            .await;
        common::expect(&reply, 201);
        reply.body["data"]["id"].as_str().unwrap().to_owned()
    }

    fn path(name: &str) -> String {
        format!("/api/v1/webhooks/{name}/signing-secret")
    }

    async fn put(&self, token: &str, name: &str, body: Json) -> common::Reply {
        self.call(Some(token), "PUT", &Self::path(name), Some(body))
            .await
    }

    async fn get(&self, token: &str, name: &str) -> common::Reply {
        self.call(Some(token), "GET", &Self::path(name), None).await
    }

    /// The service the delivery handler would take, over the running site's engine.
    async fn service(&self) -> WebhookSecretService {
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
        WebhookSecretService::new(
            self.env.site.clone(),
            live,
            Arc::new(SystemClock::new()),
            catalog,
            host,
        )
    }

    async fn finish(mut self) {
        if let Some(started) = self.started.take() {
            started.finish().await;
        }
    }
}

fn invocation(site: &str) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: Name::new("webhooks").unwrap(),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::now_utc(),
    }
}

/// Every file under `dir` that holds `needle`.
fn files_holding(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_holding(&path, needle));
        } else if let Ok(bytes) = std::fs::read(&path)
            && bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
        {
            found.push(path);
        }
    }
    found
}

// --- tests -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signing_secret_is_set_rotated_and_described_and_never_read_back() {
    let Some(rig) = Rig::new(true).await else {
        return;
    };
    let name = "orders";
    let id = rig.webhook(name).await;

    // Unset: a GET says so; the delivery handler's port has nothing to open.
    let before = rig.get(&rig.admin, name).await;
    common::expect(&before, 200);
    assert_eq!(before.body["data"]["set"], json!(false));
    let service = rig.service().await;
    assert_eq!(
        service.signing_key(&rig.env.site, name).await.unwrap_err(),
        SigningKeyError::NotSet
    );

    // Set a supplied secret: the answer has a fingerprint and no value.
    let put = rig
        .put(&rig.admin, name, json!({ "secret": SUPPLIED }))
        .await;
    common::expect(&put, 200);
    assert_eq!(put.header("cache-control"), Some("private, no-store"));
    let text = String::from_utf8_lossy(&put.bytes).into_owned();
    assert!(!text.contains(SUPPLIED), "{text}");
    assert!(put.body["data"].get("secret").is_none(), "{text}");
    let first = put.body["data"]["fingerprint"].as_str().unwrap().to_owned();
    assert_eq!(first.len(), 12);
    assert!(put.body["data"]["key_version"].as_u64().unwrap() >= 1);

    // The port opens exactly that value.
    let key = service.signing_key(&rig.env.site, name).await.unwrap();
    assert_eq!(key.expose(), SUPPLIED);

    // A rotation changes the fingerprint, and the port returns the new value.
    let rotated = rig
        .put(&rig.admin, name, json!({ "secret": ROTATED }))
        .await;
    common::expect(&rotated, 200);
    let second = rotated.body["data"]["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first, second);
    let key = service.signing_key(&rig.env.site, name).await.unwrap();
    assert_eq!(key.expose(), ROTATED);

    // A generated secret is shown once, in the PUT answer.
    let generated = rig.put(&rig.admin, name, json!({ "generate": true })).await;
    common::expect(&generated, 200);
    let secret = generated.body["data"]["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(secret.len() >= 43, "{} bytes", secret.len());
    assert_eq!(
        service
            .signing_key(&rig.env.site, name)
            .await
            .unwrap()
            .expose(),
        secret
    );
    let read = rig.get(&rig.admin, name).await;
    common::expect(&read, 200);
    assert_eq!(read.body["data"]["set"], json!(true));
    assert_eq!(
        read.body["data"]["fingerprint"],
        generated.body["data"]["fingerprint"]
    );
    let text = String::from_utf8_lossy(&read.bytes).into_owned();
    assert!(
        !text.contains(&secret) && read.body["data"].get("secret").is_none(),
        "{text}"
    );

    // No read of the webhook shows a value: the field is absent, not blank.
    let doc = rig
        .call(
            Some(&rig.admin),
            "GET",
            &format!("/api/v1/resource/webhook/{id}"),
            None,
        )
        .await;
    common::expect(&doc, 200);
    let text = String::from_utf8_lossy(&doc.bytes).into_owned();
    assert!(doc.body["data"].get("secret").is_none(), "{text}");
    for value in [SUPPLIED, ROTATED, secret.as_str()] {
        assert!(!text.contains(value), "{text}");
    }
    let list = rig
        .call(Some(&rig.admin), "GET", "/api/v1/resource/webhook", None)
        .await;
    common::expect(&list, 200);
    let text = String::from_utf8_lossy(&list.bytes).into_owned();
    for value in [SUPPLIED, ROTATED, secret.as_str()] {
        assert!(!text.contains(value), "{text}");
    }

    // The engine's journal names the caller and holds no value.
    let live = rig.started().running.live(&rig.env.site).unwrap();
    let engine = live.engine().unwrap();
    let access = engine.secret_access(SecretCaller::Webhooks, &invocation(&rig.env.site));
    let journal = engine.secret_audit_journal(&access).await.unwrap();
    let ours: Vec<_> = journal
        .iter()
        .filter(|e| e.doctype == "webhook" && e.field == "secret")
        .collect();
    assert!(
        ours.iter().filter(|e| e.op == SecretOp::Set).count() >= 3,
        "{ours:?}"
    );
    assert!(ours.iter().any(|e| e.op == SecretOp::Open), "{ours:?}");
    assert!(
        ours.iter().all(|e| e.caller == SecretCaller::Webhooks),
        "{ours:?}"
    );
    let shown = format!("{journal:?}");
    for value in [SUPPLIED, ROTATED, secret.as_str()] {
        assert!(!shown.contains(value), "the journal holds a value");
    }

    // The value is not in the database in the clear, and no file under the root holds it.
    let loaded = spot_cli::config::load(&rig.env.root.0, false).unwrap();
    let site_cfg = loaded
        .sites
        .iter()
        .find(|s| s.site == rig.env.site)
        .unwrap();
    let backend = spot_cli::schema::backend(&loaded, site_cfg).await.unwrap();
    let client = backend.pool().get().await.unwrap();
    let row = client
        .query_one("SELECT secret__sealed FROM webhook", &[])
        .await
        .unwrap();
    let sealed: Vec<u8> = row.get(0);
    for value in [SUPPLIED, ROTATED, secret.as_str()] {
        assert!(
            !sealed.windows(value.len()).any(|w| w == value.as_bytes()),
            "the sealed column holds a plain value"
        );
        assert!(
            files_holding(&rig.env.root.0, value).is_empty(),
            "a file under the root holds a signing secret"
        );
    }
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_without_the_right_is_refused_alike_and_changes_nothing() {
    let Some(rig) = Rig::new(true).await else {
        return;
    };
    rig.webhook("orders").await;
    let known = rig
        .put(&rig.plain, "orders", json!({ "secret": SUPPLIED }))
        .await;
    let unknown = rig
        .put(&rig.plain, "no_such_hook", json!({ "secret": SUPPLIED }))
        .await;
    for reply in [&known, &unknown] {
        common::expect(reply, 403);
        assert_eq!(reply.body["error"]["code"], "permission.denied");
        let text = String::from_utf8_lossy(&reply.bytes).into_owned();
        assert!(!text.contains(SUPPLIED), "{text}");
    }
    assert_eq!(
        known.body["error"]["message"], unknown.body["error"]["message"],
        "the refusal differs between a webhook that exists and one that does not"
    );
    // A GET is refused the same way, and an unsigned request is a 401.
    let read = rig.get(&rig.plain, "orders").await;
    common::expect(&read, 403);
    let unsigned = rig
        .call(
            None,
            "PUT",
            &Rig::path("orders"),
            Some(json!({ "generate": true })),
        )
        .await;
    common::expect(&unsigned, 401);
    // Nothing changed: the secret is still unset.
    let after = rig.get(&rig.admin, "orders").await;
    common::expect(&after, 200);
    assert_eq!(after.body["data"]["set"], json!(false));
    // The Administrator is told when a webhook does not exist.
    let gone = rig.get(&rig.admin, "no_such_hook").await;
    common::expect(&gone, 404);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_signing_key_is_an_error_for_an_unknown_webhook_another_site_and_a_bad_request() {
    let Some(rig) = Rig::new(true).await else {
        return;
    };
    rig.webhook("orders").await;
    rig.put(&rig.admin, "orders", json!({ "secret": SUPPLIED }))
        .await;
    let service = rig.service().await;
    assert_eq!(
        service
            .signing_key(&rig.env.site, "no_such_hook")
            .await
            .unwrap_err(),
        SigningKeyError::NotFound
    );
    assert_eq!(
        service
            .signing_key("another-site.test", "orders")
            .await
            .unwrap_err(),
        SigningKeyError::WrongSite,
        "one service never opens another site's secret"
    );
    assert_eq!(
        service
            .signing_key(&rig.env.site, "orders")
            .await
            .unwrap()
            .expose(),
        SUPPLIED
    );
    // A malformed or too-short request is refused.
    let short = rig
        .put(&rig.admin, "orders", json!({ "secret": "short" }))
        .await;
    common::expect(&short, 400);
    let both = rig
        .put(
            &rig.admin,
            "orders",
            json!({ "secret": SUPPLIED, "generate": true }),
        )
        .await;
    common::expect(&both, 400);
    // The refused writes changed nothing.
    assert_eq!(
        service
            .signing_key(&rig.env.site, "orders")
            .await
            .unwrap()
            .expose(),
        SUPPLIED
    );
    rig.finish().await;
}

// A site with no vault cannot exist any more: the sign-in stores seal their credentials through
// it (`login_stores` is required), so such a site is unavailable and never reaches this module.
// The module's own fail-closed answer is exercised by its unit tests.
#[ignore = "a site without [services.secrets] does not serve (login_stores is required)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_no_vault_fails_closed_and_cannot_sign() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    assert!(!rig.vault);
    rig.webhook("orders").await;
    for reply in [
        rig.put(&rig.admin, "orders", json!({ "generate": true }))
            .await,
        rig.get(&rig.admin, "orders").await,
    ] {
        common::expect(&reply, 501);
        assert_eq!(reply.body["error"]["code"], "service.not_configured");
    }
    // The service is degraded, and the delivery handler's port is an error, not an empty key.
    let service = rig.service().await;
    assert_eq!(
        service
            .signing_key(&rig.env.site, "orders")
            .await
            .unwrap_err(),
        SigningKeyError::NotConfigured
    );
    let ready = rig.call(None, "GET", "/api/health/ready", None).await;
    let text = String::from_utf8_lossy(&ready.bytes).into_owned();
    assert!(text.contains("webhook_secret"), "{text}");
    rig.finish().await;
}
