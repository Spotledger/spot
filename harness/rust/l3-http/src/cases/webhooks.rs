//! Outbound webhooks (WP-INT-01.T2) over the real stack: a committed event becomes a signed
//! delivery that the platform's egress guard judges, and every attempt is logged without a payload.
//!
//! The suite cannot run a public receiver (the guard refuses loopback and private addresses, which
//! is the behaviour pinned here), so this case proves the refusal half end to end: the webhook
//! targets a loopback address, `max_attempts = 1` sends it straight to the dead-letter state, the
//! log row names an error class, and neither the target URL nor any document field is in the row.
//! A delivery to a real receiver (signature, retry) is proved by the `spot-cli` and
//! `spot-webhooks` tests; it stays out of this suite until a receiver the guard allows exists.
//!
//! The case starts a host of its own and turns the services on the way an operator does: the
//! vault (key file), the `webhooks` job queue, a reload.

use std::time::{Duration, Instant};

use serde_json::json;

use super::{CaseDef, CaseFuture};
use crate::fixture::Fixture;
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After the scripts case; the case owns its fixture.
pub const ORDER: u32 = 34;

const SECTION: &str = "Outbound webhooks";

/// The services this case needs from the first start: the `webhooks` queue on the job worker and
/// a short intake poll. The worker and the outbox pump run once per process and read these
/// settings when the host starts; a reload rebuilds only the serving runtime.
const SERVICES: &str =
    "[services.jobs]\nqueues = [\"default\", \"webhooks\"]\n[services.webhooks]\npoll_ms = 100\n";

const TARGET: &str = "http://127.0.0.1:9/l3-webhook-canary";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary_with(SERVICES).await else {
            return Err(failure!(
                "the webhooks case could not start its own fixture"
            ));
        };
        let result =
            a_delivery_to_a_refused_address_dead_letters_without_payload_or_url(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "webhooks: a_delivery_to_a_refused_address_dead_letters_without_payload_or_url",
        SECTION,
        "a webhook on `project` create aimed at a loopback address with `max_attempts = 1` leaves one `webhook_delivery` row in `dead_letter` with a non-empty `error_class` and no status code, and the row and the server log carry neither the target URL nor the project's title",
        false,
        run_case,
    )]
}

async fn a_delivery_to_a_refused_address_dead_letters_without_payload_or_url(
    fixture: &Fixture,
) -> CaseResult {
    // 1. The host started with the vault (the fixture's), the `webhooks` queue and the intake
    // poll above.
    // 2. A webhook with a signing secret (nothing is ever sent unsigned).
    let admin = fixture.admin();
    let name = fixture.unique("hook");
    let created = admin
        .create(
            "webhook",
            &json!({
                "name": name,
                "url": TARGET,
                "doctype": "project",
                "events": ["create"],
                "payload_mode": "full",
                "enabled": true,
                "state": "active",
                "max_attempts": 1,
            }),
        )
        .await?;
    created.expect(201)?;
    let webhook_id = created.id()?;
    admin
        .send_json(
            "PUT",
            &format!("/api/v1/webhooks/{name}/signing-secret"),
            &[],
            &json!({"generate": true}),
        )
        .await?
        .expect(200)?;

    // 3. A committed event: the project create.
    let title = "L3 webhook canary title";
    let (_project, _, _) = fixture.project(title).await?;

    // 4. The delivery dead-letters; poll the log.
    let deadline = Instant::now() + Duration::from_secs(60);
    let row = loop {
        let listing = admin.list("webhook_delivery", "").await?;
        listing.expect(200)?;
        let found = listing.rows()?.into_iter().find(|row| {
            row["webhook"] == json!(webhook_id) && row["state"] == json!("dead_letter")
        });
        if let Some(row) = found {
            break row;
        }
        if Instant::now() > deadline {
            return Err(failure!(
                "no dead-letter delivery for the webhook after 60s: {}",
                listing.text()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    ensure!(
        row["error_class"].as_str().is_some_and(|c| !c.is_empty()),
        "the dead-letter row names no error class: {row}"
    );
    ensure!(
        row["status_code"].is_null() || row["status_code"] == json!(0),
        "a refused address cannot have an HTTP status: {row}"
    );
    let text = row.to_string();
    ensure!(
        !text.contains("l3-webhook-canary") && !text.contains(title),
        "the delivery row carries the URL or a document value: {text}"
    );
    let log = std::fs::read_to_string(fixture.log()).unwrap_or_default();
    ensure!(
        !log.contains("l3-webhook-canary") && !log.contains(title),
        "the server log carries the webhook URL or a document value"
    );
    Ok(())
}
