//! The hard limits refuse before any work, and a flood (a star of spokes) is answered inside the
//! cost budget with `truncated: true` while the server keeps serving other requests.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use tokio::sync::Semaphore;

use super::answer::{Answer, ask, lineage, query};
use super::host::{FAST, GraphHost, ORDER_T, RECEIPT_T, Start, UNKNOWN_ID};
use crate::client::Client;
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// How many spokes the star has unless `L3_GRAPH_STAR` says otherwise (1,001 to 100,000). Every
/// spoke is a real receipt created over HTTP, so the brief's 100,000 is a nightly setting, not
/// the default.
const STAR: usize = 1_200;

pub async fn the_hard_limits_refuse_first_and_a_flood_is_answered_in_budget() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        ..Start::default()
    })
    .await?;
    let result = run(&host).await;
    host.finish().await;
    result
}

async fn run(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    refused(admin).await?;
    star(host).await
}

fn nested_not(levels: usize) -> Json {
    let mut filter = json!({"field": "note", "op": "eq", "value": "x"});
    for _ in 0..levels {
        filter = json!({"not": filter});
    }
    filter
}

/// Forty terms in three levels: eight `and`s of five comparisons.
fn many_terms() -> Json {
    let leaf = json!({"field": "note", "op": "eq", "value": "x"});
    let group = json!({"and": [leaf, leaf, leaf, leaf, leaf]});
    json!({"and": [group, group, group, group, group, group, group, group]})
}

/// Every refusal is asked about a record that does not exist: a limit is answered before any
/// lookup, so the code is the limit's and never `graph.not_found`.
async fn refused(admin: &Client) -> CaseResult {
    let mut limit = query(ORDER_T, UNKNOWN_ID, "both", 2, &[]);
    limit["limit"] = json!(1001);
    let types: Vec<String> = (0..17).map(|n| format!("edge_type_{n}")).collect();
    let refs: Vec<&str> = types.iter().map(String::as_str).collect();
    let mut deep = query(ORDER_T, UNKNOWN_ID, "both", 2, &[]);
    deep["node_filter"] = nested_not(6);
    let mut wide = query(ORDER_T, UNKNOWN_ID, "both", 2, &[]);
    wide["node_filter"] = many_terms();
    // Faults of another kind: invalid, not too large, and the value is never echoed.
    let mut sideways = query(ORDER_T, UNKNOWN_ID, "sideways-VALUE-91", 2, &[]);
    sideways["limit"] = json!(10);
    let cases: Vec<(&str, Json, &str)> = vec![
        (
            "depth 9",
            query(ORDER_T, UNKNOWN_ID, "both", 9, &[]),
            "graph.too_large",
        ),
        ("limit 1001", limit, "graph.too_large"),
        (
            "17 edge types",
            query(ORDER_T, UNKNOWN_ID, "both", 2, &refs),
            "graph.too_large",
        ),
        ("a filter nested 6 deep", deep, "graph.too_large"),
        ("a filter of 40 terms", wide, "graph.too_large"),
        ("an unknown direction", sideways, "graph.query_invalid"),
        (
            "depth 0",
            query(ORDER_T, UNKNOWN_ID, "both", 0, &[]),
            "graph.query_invalid",
        ),
        (
            "no start",
            json!({"direction": "both", "depth": 2, "limit": 10}),
            "graph.query_invalid",
        ),
    ];
    for (what, body, code) in cases {
        let started = Instant::now();
        let reply = ask(admin, &body).await?;
        reply.expect(422)?.expect_error(code)?;
        ensure!(
            !reply.text().contains("VALUE-91"),
            "the refusal of {what} echoes the submitted value: {}",
            reply.text()
        );
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "the refusal of {what} took {:?}",
            started.elapsed()
        );
    }
    // Lineage has its own ceilings: depth 12 and 5,000 nodes.
    for (what, depth, limit) in [
        ("lineage depth 13", Some(13), None),
        ("lineage limit 5001", None, Some(5001)),
    ] {
        let reply = lineage(admin, "chain", ORDER_T, UNKNOWN_ID, depth, limit).await?;
        reply.expect(422)?.expect_error("graph.too_large")?;
        ensure!(reply.status == 422, "{what} was not refused");
    }
    let kind = lineage(admin, "sideways", ORDER_T, UNKNOWN_ID, None, None).await?;
    kind.expect(422)?.expect_error("lineage.kind_unknown")?;
    Ok(())
}

fn star_size() -> usize {
    std::env::var("L3_GRAPH_STAR")
        .ok()
        .and_then(|text| text.parse::<usize>().ok())
        .map_or(STAR, |n| n.clamp(1_001, 100_000))
}

/// A star: one order and many receipts of it. A walk from the hub with a limit of 1,000 reaches
/// the limit at once.
async fn star(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    let (hub, _) = host.make(admin, ORDER_T, &json!({"ref": "HUB"})).await?;
    let spokes = star_size();
    let gate = Arc::new(Semaphore::new(16));
    let flood = admin.with_in_flight_gate(gate);
    let mut set = tokio::task::JoinSet::new();
    for n in 0..spokes {
        let client = flood.clone();
        let body = json!({"source": hub, "note": format!("spoke-{n}")});
        set.spawn(async move {
            let reply = client.create(RECEIPT_T, &body).await?;
            reply.expect(201)?;
            Ok::<(), Failure>(())
        });
    }
    while let Some(done) = set.join_next().await {
        done.map_err(|e| failure!("a create task failed: {e}"))??;
    }

    // The index follows the writes within a feed interval: wait until it holds the star (the
    // answer reaches the node limit) before the flood query is timed.
    let caught_up = Instant::now() + Duration::from_secs(60);
    loop {
        let body = query(ORDER_T, &hub, "in", 1, &[]);
        let reply = ask(admin, &body).await?;
        if reply.status == 200 && Answer::from(&reply)?.truncated {
            break;
        }
        ensure!(
            Instant::now() < caught_up,
            "the index never held the star of {spokes} spokes"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let started = Instant::now();
    let heavy = {
        let client = admin.clone();
        let body = query(ORDER_T, &hub, "in", 1, &[]);
        tokio::spawn(async move { ask(&client, &body).await })
    };
    // The server answers other requests while the flood is walked: each ping is a plain read.
    let mut pings = 0;
    loop {
        let began = Instant::now();
        let reply = admin.document(ORDER_T, &hub).await?;
        reply.expect(200)?;
        ensure!(
            began.elapsed() < Duration::from_secs(5),
            "a plain read took {:?} while the flood was walked",
            began.elapsed()
        );
        pings += 1;
        if heavy.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reply = heavy
        .await
        .map_err(|e| failure!("the flood query task failed: {e}"))??;
    let took = started.elapsed();
    let found = Answer::from(&reply)?;
    ensure!(
        found.truncated && found.reason.as_deref() == Some("nodes"),
        "a star of {spokes} spokes is not answered as truncated by `nodes`: {} nodes, truncated {}, reason {:?}",
        found.nodes.len(),
        found.truncated,
        found.reason
    );
    ensure!(
        found.nodes.len() <= 1_001,
        "the answer holds {} nodes, more than the limit allows",
        found.nodes.len()
    );
    ensure!(
        took < Duration::from_secs(15),
        "the flood took {took:?} ({pings} pings answered meanwhile), beyond the cost budget"
    );
    Ok(())
}
