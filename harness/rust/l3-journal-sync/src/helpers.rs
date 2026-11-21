//! Read-only helpers other tasks need: a polling wait, generic SQL readers over a node's own
//! database, and a content fingerprint for comparing two databases.
//!
//! `read_conditions` and `read_audit` read tables the hosted journal/sync tasks create
//! (`spot_conditions`, `spot_audit_log`); this crate (WP-RTB-01.T1) adds no journal or sync
//! behaviour, so on a plain node neither table exists yet. A missing table is not an error: the
//! helpers return an empty list (checked by the server's own `undefined_table`, SQLSTATE
//! `42P01`, not by guessing which tables exist), so the call already reads correctly once a
//! later task's migration creates the table, with no change needed here.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::{Map, Value as Json};

use crate::Node;

/// Polls `cond` every 50 ms until it returns `true` or `timeout` elapses.
///
/// # Errors
/// A clear message naming the timeout when `cond` never became true.
pub async fn wait_until<F, Fut>(timeout: Duration, mut cond: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if cond().await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("condition was not met within {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One row, decoded generically through PostgreSQL's own `row_to_json`: every column becomes a
/// JSON member, so this crate never has to know a later task's column types.
pub type Row = Map<String, Json>;

/// The SQLSTATE PostgreSQL reports for a query against a table that does not exist.
const UNDEFINED_TABLE: &str = "42P01";

/// Runs `query` against `node`'s own database and decodes every row through `row_to_json`,
/// returning an empty list (not an error) when the named table does not exist yet.
async fn read_rows(node: &Node, query: &str) -> Result<Vec<Row>, String> {
    let client = node.raw_client().await;
    let wrapped = format!("SELECT row_to_json(t)::text AS doc FROM ({query}) t");
    match client.query(&wrapped, &[]).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| {
                let text: String = row.get(0);
                match serde_json::from_str::<Json>(&text) {
                    Ok(Json::Object(map)) => Ok(map),
                    Ok(other) => Err(format!("row_to_json did not return an object: {other}")),
                    Err(e) => Err(format!("row_to_json produced invalid JSON: {e}")),
                }
            })
            .collect(),
        Err(error) => {
            if error
                .code()
                .is_some_and(|code| code.code() == UNDEFINED_TABLE)
            {
                Ok(Vec::new())
            } else {
                Err(format!(
                    "query against node `{}` failed: {error}",
                    node.name()
                ))
            }
        }
    }
}

/// Every row of `spot_conditions` on `node`'s own database, oldest first; an empty list before
/// the journal hosting task (later in WP-RTB-01) creates the table.
pub async fn read_conditions(node: &Node) -> Result<Vec<Row>, String> {
    read_rows(node, "SELECT * FROM spot_conditions ORDER BY 1").await
}

/// Every row of `spot_audit_log` on `node`'s own database whose `topic` column is `topic`,
/// oldest first; an empty list before a later task creates the table.
pub async fn read_audit(node: &Node, topic: &str) -> Result<Vec<Row>, String> {
    let client = node.raw_client().await;
    let wrapped = "SELECT row_to_json(t)::text AS doc FROM \
                   (SELECT * FROM spot_audit_log WHERE topic = $1 ORDER BY 1) t";
    match client.query(wrapped, &[&topic]).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| {
                let text: String = row.get(0);
                match serde_json::from_str::<Json>(&text) {
                    Ok(Json::Object(map)) => Ok(map),
                    Ok(other) => Err(format!("row_to_json did not return an object: {other}")),
                    Err(e) => Err(format!("row_to_json produced invalid JSON: {e}")),
                }
            })
            .collect(),
        Err(error) => {
            if error
                .code()
                .is_some_and(|code| code.code() == UNDEFINED_TABLE)
            {
                Ok(Vec::new())
            } else {
                Err(format!(
                    "audit query against node `{}` failed: {error}",
                    node.name()
                ))
            }
        }
    }
}

/// A stable content hash over every captured table of `node`'s database: every table in the
/// `public` schema, in alphabetical order, each table's rows decoded with `row_to_json` and
/// sorted lexicographically (so the hash does not depend on row order), fed into
/// [`spot_journal_store::etag`]. Two nodes (or the same node before and after a change)
/// compare equal exactly when every table's content is identical.
///
/// This reads the database directly, independent of whether the host is serving: a running
/// host's own background services (the scheduler, the metadata poller) write to it on their
/// own schedule, so two fingerprints taken while it serves are not guaranteed to agree even
/// with nothing the caller did between them. Call it with the node stopped
/// ([`crate::Node::stop`]) for a comparison that only reflects data a caller wrote.
///
/// # Errors
/// A plain message when the table list or a table's rows cannot be read.
pub async fn fingerprint(node: &Node) -> Result<String, String> {
    let client = node.raw_client().await;
    let tables = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
             ORDER BY table_name",
            &[],
        )
        .await
        .map_err(|e| format!("cannot list node `{}`'s tables: {e}", node.name()))?;
    let mut buffer = String::new();
    for table in tables {
        let name: String = table.get(0);
        // `name` is one of this database's own catalogued table names, never caller input.
        let query = format!("SELECT * FROM \"{name}\"");
        let mut rows = read_rows(node, &query).await?;
        let mut texts: Vec<String> = rows
            .drain(..)
            .map(|row| Json::Object(row).to_string())
            .collect();
        texts.sort();
        buffer.push_str(&name);
        buffer.push('\n');
        for text in texts {
            buffer.push_str(&text);
            buffer.push('\n');
        }
    }
    Ok(spot_journal_store::etag(buffer.as_bytes()))
}
