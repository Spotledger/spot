//! The event stream (`/api/v1/events/*`, WP-INT-02.T4): a named consumer reads the events of the
//! documents the cases write, in order, acknowledges what it applied and replays from a stored
//! position; a user without the right is denied.
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host,
//! so these cases cannot use the fixture they are handed. They start their own (one more
//! database and host, a few seconds) and finish it.

use serde_json::{Value as Json, json};

use super::{CaseDef, CaseFuture};
use crate::client::Reply;
use crate::fixture::{Fixture, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// Runs after `export` (the order the cases ran in before the registry).
pub const ORDER: u32 = 20;

const SECTION: &str = "The event stream";

fn run_replay(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start().await else {
            return Err(failure!("the events case could not start its own fixture"));
        };
        let result = a_consumer_reads_in_order_acknowledges_and_replays(&own).await;
        let denied = a_user_without_the_stream_right_is_denied(&own).await;
        own.finish().await;
        result.and(denied)
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "events: a_consumer_reads_in_order_acknowledges_replays_and_denies_a_viewer",
            SECTION,
            "`/events/consumers` put, pull (a repeat returns the same events), ack, restart from the beginning replays; a Viewer's `GET /events/consumers` is 403 `permission.denied`, not an empty list",
            false,
            run_replay,
        ),
    ]
}

const CONSUMER: &str = "/api/v1/events/consumers/l3_events";

fn subjects(reply: &Reply) -> Vec<String> {
    reply.json()["data"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e["subject"]["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn event_ids(reply: &Reply) -> Vec<String> {
    reply.json()["data"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e["event_id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

async fn a_consumer_reads_in_order_acknowledges_and_replays(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let spec = json!({
        "topics": ["doc_created"],
        "filter": "doc.subject.entity == 'project'",
        "partitions": 1,
        "payload_mode": "ids_only",
        "start": {"kind": "tail"},
    });
    admin
        .send_json("PUT", CONSUMER, &[], &spec)
        .await?
        .expect(201)?;
    let (first, _, _) = fixture.project("Events one").await?;
    let (second, _, _) = fixture.project("Events two").await?;

    let pull = json!({"partition": 0, "limit": 100, "wait_ms": 5000});
    let batch = admin
        .send_json("POST", &format!("{CONSUMER}/pull"), &[], &pull)
        .await?;
    batch.expect(200)?;
    let lease = batch
        .header("x-consumer-lease")
        .map(str::to_owned)
        .ok_or_else(|| failure!("the first pull answered no X-Consumer-Lease"))?;
    ensure!(
        subjects(&batch) == vec![first.clone(), second.clone()],
        "the consumer should read the two projects in order, got {}",
        batch.text()
    );
    let headers = [("x-consumer-lease", lease.as_str())];
    // A pull that is repeated before an ack returns the same events with the same ids.
    let again = admin
        .send_json("POST", &format!("{CONSUMER}/pull"), &headers, &pull)
        .await?;
    again.expect(200)?;
    ensure!(
        event_ids(&again) == event_ids(&batch),
        "a repeated pull returned other events: {}",
        again.text()
    );
    // Only an ack moves the checkpoint.
    let next: Json = batch.json()["data"]["next_position"].clone();
    admin
        .send_json(
            "POST",
            &format!("{CONSUMER}/ack"),
            &headers,
            &json!({"partition": 0, "position": next}),
        )
        .await?
        .expect(200)?;
    let drained = admin
        .send_json(
            "POST",
            &format!("{CONSUMER}/pull"),
            &headers,
            &json!({"partition": 0}),
        )
        .await?;
    drained.expect(200)?;
    ensure!(
        subjects(&drained).is_empty(),
        "acknowledged events were delivered again: {}",
        drained.text()
    );
    // A restart from the beginning replays them, from a stored position.
    admin
        .send_json(
            "POST",
            &format!("{CONSUMER}/restart"),
            &headers,
            &json!({"partition": 0, "from": {"kind": "from_beginning"}}),
        )
        .await?
        .expect(200)?;
    let replay = admin
        .send_json("POST", &format!("{CONSUMER}/pull"), &headers, &pull)
        .await?;
    replay.expect(200)?;
    // From the stored position: each of the two events once and in order (the replay also holds
    // later audit events of other kinds, which this pin does not depend on).
    let ours: Vec<String> = subjects(&replay)
        .into_iter()
        .filter(|id| *id == first || *id == second)
        .collect();
    ensure!(
        ours == vec![first.clone(), second.clone()],
        "the replay is not the projects written before the restart, once each and in order: {}",
        replay.text()
    );
    admin
        .send("DELETE", CONSUMER, &[], None)
        .await?
        .expect(204)?;
    Ok(())
}

async fn a_user_without_the_stream_right_is_denied(fixture: &Fixture) -> CaseResult {
    let viewer = fixture.sign_in(VIEWER).await?;
    for (method, path) in [
        ("GET", "/api/v1/events/consumers"),
        ("GET", "/api/v1/events/consumers/l3_events"),
    ] {
        let reply = viewer.send(method, path, &[], None).await?;
        reply.expect_error("permission.denied")?;
        ensure!(
            reply.status == 403,
            "{method} {path} answered HTTP {}, not 403: {}",
            reply.status,
            reply.text()
        );
    }
    Ok(())
}
