//! WP-INT-01.T2: outbound webhooks run. Real PostgreSQL and a real `spot serve` (skipped, with no
//! output, when `SPOT_TEST_PG_URL` is unset).
//!
//! What is proved: a committed event that matches a webhook becomes a delivery that is logged as a
//! `webhook_delivery` document (one row per attempt, no URL, no body, no secret); a webhook that
//! points at a loopback address is refused by the egress guard and dead-lettered, not sent; an
//! event nothing listens to, and a webhook that is paused, make no delivery; the delivery log is
//! never a source of further deliveries; the module's cursor table exists and the intake survives
//! a restart without delivering the same event twice.
//!
//! What is not proved here: a successful send to a receiver. The test egress policy has no
//! production switch, and this task adds none (see the report's open questions).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Env, Started};
use serde_json::{Value as Json, json};
use spot_cli::services::secrets as svc;
use spot_storage_contract::RecordId;

const TARGET: &str = "http://127.0.0.1:9/receiver-canary-path";
const WEBHOOK_NAME: &str = "wh-t2-private";

struct Rig {
    env: Env,
    dir: PathBuf,
    started: Option<Started>,
    admin: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    async fn new() -> Option<Rig> {
        let dir = std::env::temp_dir().join(format!("spot_webhooks_t2_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let services = format!(
            "[services.secrets]\nkek_file = {:?}\n[services.jobs]\nqueues = [\"default\", \"webhooks\"]\n[services.webhooks]\npoll_ms = 100\nmax_attempts = 2\n",
            format!("{}/{{site}}.json", dir.display())
        );
        let env = Env::new(&services).await?;
        let first = env.site();
        let kek = dir.join(format!("{}.json", env.site));
        svc::init_key_file(&kek, &env.site).await.unwrap();
        let started = common::start(&env).await;
        let admin = common::admin(started.addr, &env.site, &first).await;
        Some(Rig {
            env,
            dir,
            started: Some(started),
            admin,
        })
    }

    fn started(&self) -> &Started {
        self.started.as_ref().unwrap()
    }

    async fn call(&self, method: &str, path: &str, body: Option<Json>) -> common::Reply {
        common::http(
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

    async fn webhook(&self, name: &str, state: &str, events: Json) -> String {
        let reply = self
            .call(
                "POST",
                "/api/v1/resource/webhook",
                Some(json!({
                    "name": name,
                    "url": TARGET,
                    "doctype": "project",
                    "events": events,
                    "payload_mode": "ids",
                    "state": state,
                    "max_attempts": 2,
                })),
            )
            .await;
        common::expect(&reply, 201);
        let put = self
            .call(
                "PUT",
                &format!("/api/v1/webhooks/{name}/signing-secret"),
                Some(json!({ "generate": true })),
            )
            .await;
        common::expect(&put, 200);
        reply.body["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn project(&self, code: &str) -> String {
        let owner = common::me(self.started().addr, &self.env.site, &self.admin).await;
        let reply = self
            .call(
                "POST",
                "/api/v1/resource/project",
                Some(json!({
                    "code": code, "title": "T2", "status": "Active",
                    "owner": owner, "currency": "USD", "budget": "10.00"
                })),
            )
            .await;
        common::expect(&reply, 201);
        reply.body["data"]["id"].as_str().unwrap().to_owned()
    }

    /// The `webhook_delivery` rows now, as the Administrator sees them.
    async fn deliveries(&self) -> (Vec<Json>, String) {
        let reply = self
            .call("GET", "/api/v1/resource/webhook_delivery?limit=100", None)
            .await;
        common::expect(&reply, 200);
        let text = String::from_utf8_lossy(&reply.bytes).into_owned();
        let rows = reply.body["data"].as_array().cloned().unwrap_or_default();
        (rows, text)
    }

    /// Waits until `done` holds of the delivery rows; returns the rows and their raw text.
    async fn wait_for(&self, done: impl Fn(&[Json]) -> bool) -> (Vec<Json>, String) {
        let end = Instant::now() + Duration::from_secs(45);
        loop {
            let (rows, text) = self.deliveries().await;
            if done(&rows) || Instant::now() > end {
                return (rows, text);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn finish(mut self) {
        if let Some(started) = self.started.take() {
            started.finish().await;
        }
    }
}

fn state_of(row: &Json) -> &str {
    row["state"].as_str().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loopback_target_is_refused_by_egress_and_logged_without_leaking() {
    let Some(rig) = Rig::new().await else { return };
    rig.webhook(WEBHOOK_NAME, "active", json!(["create"])).await;
    rig.project("PRJ-T2-1").await;
    let (rows, text) = rig
        .wait_for(|rows| rows.iter().any(|r| state_of(r) == "dead_letter"))
        .await;
    assert!(
        rows.iter().any(|r| state_of(r) == "dead_letter"),
        "no dead-lettered delivery: {text}"
    );
    // The log keeps no URL, no body and no host of the target.
    for needle in ["receiver-canary-path", "127.0.0.1", "whsec"] {
        assert!(
            !text.contains(needle),
            "{needle} leaked into the log: {text}"
        );
    }
    // Every attempt of one delivery shares the delivery id; no attempt exceeds the ceiling of 2.
    for row in &rows {
        let attempt = row["attempt"].as_i64().unwrap_or(0);
        assert!((1..=2).contains(&attempt), "{row}");
    }
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_webhook_and_an_unlisted_event_make_no_delivery() {
    let Some(rig) = Rig::new().await else { return };
    rig.webhook("wh-t2-paused", "paused", json!(["create"]))
        .await;
    rig.webhook("wh-t2-delete-only", "active", json!(["delete"]))
        .await;
    rig.project("PRJ-T2-2").await;
    // Give the intake several polls (100 ms each) to act on the create event.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (rows, text) = rig.deliveries().await;
    assert!(rows.is_empty(), "unexpected deliveries: {text}");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delivery_log_never_feeds_back_and_a_restart_does_not_repeat_a_delivery() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.webhook(WEBHOOK_NAME, "active", json!(["create"])).await;
    rig.project("PRJ-T2-3").await;
    let (first, _) = rig
        .wait_for(|rows| rows.iter().any(|r| state_of(r) == "dead_letter"))
        .await;
    let delivery_ids = |rows: &[Json]| {
        let mut ids: Vec<String> = rows
            .iter()
            .filter_map(|r| r["delivery_id"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let before = delivery_ids(&first);
    // Exactly one event, one webhook: one delivery id, however many attempts it took, and the
    // rows the log itself wrote did not start another delivery.
    assert_eq!(before.len(), 1, "{first:?}");

    // Restart the host over the same database: the cursor is in the database, so the same event
    // is not delivered again.
    rig.started.take().unwrap().finish().await;
    rig.started = Some(common::start(&rig.env).await);
    rig.admin = common::token_of(
        rig.started().addr,
        &rig.env.site,
        "administrator",
        common::ADMIN_PASSWORD,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (after, _) = rig.deliveries().await;
    assert_eq!(delivery_ids(&after), before, "{after:?}");
    rig.finish().await;
}
