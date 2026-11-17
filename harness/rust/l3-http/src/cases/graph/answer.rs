//! Questions to `POST /api/v1/graph/query` and `POST /api/v1/lineage`, and their answers read as
//! sets, so a case compares what was found and not how it was ordered.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use crate::client::{Client, Reply};
use crate::report::Failure;
use crate::{ensure, failure};

/// The answer of `graph_query`: node ids, edges as `(from id, to id, type)`, and the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub nodes: BTreeSet<String>,
    pub edges: BTreeSet<(String, String, String)>,
    pub truncated: bool,
    pub reason: Option<String>,
    pub withheld: bool,
}

impl Answer {
    /// Reads a `200` answer.
    pub fn from(reply: &Reply) -> Result<Answer, Failure> {
        reply.expect(200)?;
        let data = reply.data();
        let nodes = data
            .get("nodes")
            .and_then(Json::as_array)
            .ok_or_else(|| failure!("the answer has no `nodes`: {}", reply.text()))?
            .iter()
            .filter_map(|node| node.get("id").and_then(Json::as_str).map(str::to_owned))
            .collect();
        let edges = data
            .get("edges")
            .and_then(Json::as_array)
            .ok_or_else(|| failure!("the answer has no `edges`: {}", reply.text()))?
            .iter()
            .map(|edge| {
                let end = |side: &str| {
                    edge.pointer(&format!("/{side}/id"))
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                let kind = edge
                    .get("type")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned();
                (end("from"), end("to"), kind)
            })
            .collect();
        Ok(Answer {
            nodes,
            edges,
            truncated: data.get("truncated").and_then(Json::as_bool) == Some(true),
            reason: data
                .get("truncation_reason")
                .and_then(Json::as_str)
                .map(str::to_owned),
            withheld: data.get("withheld").and_then(Json::as_bool) == Some(true),
        })
    }

    /// The node ids as a set built from `ids`.
    pub fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }
}

/// A graph query body: `direction` is `out` (to the records a record links to), `in` (to the
/// records that link to it) or `both`; no `types` means every edge type.
pub fn query(doctype: &str, id: &str, direction: &str, depth: u64, types: &[&str]) -> Json {
    let mut body = json!({
        "start": {"doctype": doctype, "id": id},
        "direction": direction,
        "depth": depth,
        "limit": 1000,
    });
    if !types.is_empty() {
        body["edge_types"] = json!(types);
    }
    body
}

/// Asks the graph.
pub async fn ask(client: &Client, body: &Json) -> Result<Reply, Failure> {
    client
        .send_json("POST", "/api/v1/graph/query", &[], body)
        .await
}

/// Asks the graph and reads a `200` answer.
pub async fn answer(client: &Client, body: &Json) -> Result<Answer, Failure> {
    Answer::from(&ask(client, body).await?)
}

/// Asks until the answer satisfies `done` (30 s at most): the index follows a write within a feed
/// interval, the database path at once.
pub async fn until(
    client: &Client,
    body: &Json,
    what: &str,
    done: impl Fn(&Answer) -> bool,
) -> Result<Answer, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = ask(client, body).await?;
        let last = Answer::from(&reply)?;
        if done(&last) {
            return Ok(last);
        }
        ensure!(
            Instant::now() < deadline,
            "the graph answer never became `{what}`: {}",
            reply.text()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The error object of a reply without the request id, so two refusals compare byte for byte.
pub fn error_of(reply: &Reply) -> Json {
    let mut error = reply.json().get("error").cloned().unwrap_or(Json::Null);
    if let Some(map) = error.as_object_mut() {
        map.remove("request_id");
    }
    error
}

/// Asks `lineage_trace`.
pub async fn lineage(
    client: &Client,
    kind: &str,
    doctype: &str,
    id: &str,
    depth: Option<u64>,
    limit: Option<u64>,
) -> Result<Reply, Failure> {
    let mut body = json!({"kind": kind, "doctype": doctype, "id": id});
    if let Some(depth) = depth {
        body["depth"] = json!(depth);
    }
    if let Some(limit) = limit {
        body["limit"] = json!(limit);
    }
    client
        .send_json("POST", "/api/v1/lineage", &[], &body)
        .await
}

/// Asks `lineage_trace` until the answer satisfies `done` (30 s at most): the hot index follows a
/// write within a feed interval, so a question asked at once may not see the last write yet.
pub async fn lineage_until(
    client: &Client,
    kind: &str,
    doctype: &str,
    id: &str,
    depth: Option<u64>,
    done: impl Fn(&Lineage) -> bool,
) -> Result<Lineage, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = lineage(client, kind, doctype, id, depth, None).await?;
        let last = Lineage::from(&reply)?;
        if done(&last) || Instant::now() >= deadline {
            return Ok(last);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The answer of `lineage_trace`.
#[derive(Debug, Clone)]
pub struct Lineage {
    pub levels: Vec<Json>,
    pub truncated: bool,
    pub reason: Option<String>,
    pub withheld: bool,
}

impl Lineage {
    /// Reads a `200` answer.
    pub fn from(reply: &Reply) -> Result<Lineage, Failure> {
        reply.expect(200)?;
        let data = reply.data();
        let levels = data
            .get("levels")
            .and_then(Json::as_array)
            .cloned()
            .ok_or_else(|| failure!("the answer has no `levels`: {}", reply.text()))?;
        Ok(Lineage {
            levels,
            truncated: data.get("truncated").and_then(Json::as_bool) == Some(true),
            reason: data
                .get("truncation_reason")
                .and_then(Json::as_str)
                .map(str::to_owned),
            withheld: data.get("withheld").and_then(Json::as_bool) == Some(true),
        })
    }

    /// Every node id of every level.
    pub fn ids(&self) -> BTreeSet<String> {
        (0..self.levels.len())
            .flat_map(|d| self.level_ids(d))
            .collect()
    }

    /// The node ids of one level (empty when there is no such level).
    pub fn level_ids(&self, depth: usize) -> BTreeSet<String> {
        self.nodes(depth)
            .iter()
            .filter_map(|node| node.get("id").and_then(Json::as_str).map(str::to_owned))
            .collect()
    }

    /// The nodes of one level.
    pub fn nodes(&self, depth: usize) -> Vec<Json> {
        self.levels
            .iter()
            .find(|level| level.get("depth").and_then(Json::as_u64) == Some(depth as u64))
            .and_then(|level| level.get("nodes").and_then(Json::as_array).cloned())
            .unwrap_or_default()
    }

    /// The `quantity_total` of one level.
    pub fn total(&self, depth: usize) -> Option<Json> {
        self.levels
            .iter()
            .find(|level| level.get("depth").and_then(Json::as_u64) == Some(depth as u64))
            .and_then(|level| level.get("quantity_total").cloned())
    }
}

/// A decimal text without trailing zeros (`"0.30"` and `"0.3"` are one value), so a case
/// compares the value and not the scale the server printed.
pub fn norm(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text.to_owned()
    }
}

/// A decimal that must be a JSON string (never a number), normalised.
pub fn exact(value: &Json, what: &str) -> Result<String, Failure> {
    match value.as_str() {
        Some(text) => Ok(norm(text)),
        None => Err(failure!("{what} is not a decimal string: {value}")),
    }
}
