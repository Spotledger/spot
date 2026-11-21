//! The sinks: the write-only seam to an external graph engine and its two reference
//! implementations.
//!
//! [`GraphSink`] is the whole contract: apply a batch, report the sequence it has applied, and
//! reset. It is **write-only** — it never reads storage and never obtains a transaction — so a
//! projection can never write back into the business database (ADR-50). [`SinkReader`] is the
//! separate, read-only view the drift verifier uses; the reference sinks implement both, a real
//! external engine implements the writer and answers reads through its own query API.
//!
//! [`InMemorySink`] holds a [`GraphIndex`] (the same structure the in-memory index uses), so its
//! semantics are the index's: strictly increasing sequences, duplicates ignored, a hole reported
//! and isolated endpoints dropped. [`FileSink`] is the same index plus a JSON-lines log for
//! tests and export; [`replay`] rebuilds an index from that log.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde_json::Value as Json;
use spot_graph_index::{Edge, EdgeEvent, EdgeKey, EdgeOp, GraphIndex, GraphKey};
use spot_storage_contract::Value;

use crate::error::ProjectionError;
use crate::wire::{
    edge_from_json, edge_to_json, event_from_json, event_to_json, kind_of, snapshot_to_json,
    snapshot_watermark,
};

/// A batch handed to a [`GraphSink`]: either incremental edge events, or a full snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeBatch {
    graph: GraphKey,
    kind: BatchKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchKind {
    Events(Vec<EdgeEvent>),
    Snapshot { watermark: u64, edges: Vec<Edge> },
}

impl EdgeBatch {
    /// A batch of incremental edge events for `graph`, in order.
    #[must_use]
    pub fn events(graph: GraphKey, events: Vec<EdgeEvent>) -> Self {
        Self {
            graph,
            kind: BatchKind::Events(events),
        }
    }

    /// A full snapshot of `graph` as of `watermark`: the sink replaces its contents.
    #[must_use]
    pub fn snapshot(graph: GraphKey, watermark: u64, edges: Vec<Edge>) -> Self {
        Self {
            graph,
            kind: BatchKind::Snapshot { watermark, edges },
        }
    }

    /// The graph the batch is for.
    #[must_use]
    pub fn graph(&self) -> &GraphKey {
        &self.graph
    }

    /// Whether the batch is a full snapshot.
    #[must_use]
    pub fn is_snapshot(&self) -> bool {
        matches!(self.kind, BatchKind::Snapshot { .. })
    }

    /// The incremental events; empty for a snapshot.
    #[must_use]
    pub fn incremental_events(&self) -> &[EdgeEvent] {
        match &self.kind {
            BatchKind::Events(events) => events,
            BatchKind::Snapshot { .. } => &[],
        }
    }

    /// The snapshot's watermark and edges, when the batch is a snapshot.
    #[must_use]
    pub fn snapshot_parts(&self) -> Option<(u64, &[Edge])> {
        match &self.kind {
            BatchKind::Snapshot { watermark, edges } => Some((*watermark, edges)),
            BatchKind::Events(_) => None,
        }
    }
}

/// What applying a batch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SinkOutcome {
    /// Events that changed the sink (an added, updated or removed edge).
    pub changed: u32,
    /// Events that were already reflected (a duplicate or an older sequence).
    pub ignored: u32,
}

/// The write-only seam to an external graph engine.
///
/// A sink is **write-only**: it has no read method and no way to obtain a storage transaction, so
/// a projection can never write back into the business database (ADR-50).
///
/// ```compile_fail
/// # use spot_graph_projection::GraphSink;
/// # use spot_storage_contract::Transaction;
/// fn smuggle(sink: &dyn GraphSink) -> Box<dyn Transaction> {
///     sink.transaction() // no such method: a sink cannot obtain a `Tx`
/// }
/// ```
pub trait GraphSink: Send + Sync {
    /// Applies a batch, in order, and reports what changed.
    ///
    /// # Errors
    ///
    /// [`ProjectionError`] when the sink refuses the batch (a wrong graph, a hole, a failure).
    fn apply(&self, batch: &EdgeBatch) -> Result<SinkOutcome, ProjectionError>;

    /// The highest source sequence the sink has applied. 0 before anything.
    fn checkpoint(&self) -> u64;

    /// Clears the sink, as a rebuild does before it streams a snapshot.
    ///
    /// # Errors
    ///
    /// [`ProjectionError`] when the sink cannot be cleared.
    fn reset(&self) -> Result<(), ProjectionError>;
}

/// The read-only view of a sink, for the drift verifier.
pub trait SinkReader: Send + Sync {
    /// The graph the sink holds.
    fn key(&self) -> GraphKey;

    /// Every edge the sink holds, in any order.
    fn edges(&self) -> Vec<Edge>;

    /// The highest sequence the sink has applied. 0 before anything.
    fn watermark(&self) -> u64;
}

/// Applies `batch` to `index`, returning the outcome and the wire lines to log (empty for the
/// in-memory sink).
fn apply_into(
    index: &mut GraphIndex,
    key: &GraphKey,
    batch: &EdgeBatch,
) -> Result<(SinkOutcome, Vec<Json>), ProjectionError> {
    if batch.graph() != key {
        return Err(ProjectionError::WrongGraph {
            expected: key.clone(),
            got: batch.graph().clone(),
        });
    }
    match &batch.kind {
        BatchKind::Events(events) => {
            let mut changed = 0u32;
            let mut ignored = 0u32;
            let mut lines = Vec::new();
            for event in events {
                let applied = index.apply(event).map_err(ProjectionError::from)?;
                if applied.changed() {
                    changed += 1;
                    lines.push(event_to_json(event));
                } else {
                    ignored += 1;
                }
            }
            Ok((SinkOutcome { changed, ignored }, lines))
        }
        BatchKind::Snapshot { watermark, edges } => {
            let rebuilt = GraphIndex::build(key.clone(), *watermark, edges.clone())
                .map_err(ProjectionError::from)?;
            *index = rebuilt;
            let mut lines = Vec::with_capacity(edges.len() + 1);
            lines.push(snapshot_to_json(key, *watermark));
            for edge in edges {
                lines.push(edge_to_json(edge));
            }
            let changed = u32::try_from(edges.len()).unwrap_or(u32::MAX);
            Ok((
                SinkOutcome {
                    changed,
                    ignored: 0,
                },
                lines,
            ))
        }
    }
}

/// A sink that keeps the graph in memory, for tests and the single-node reference host.
#[derive(Debug)]
pub struct InMemorySink {
    key: GraphKey,
    index: RwLock<GraphIndex>,
}

impl InMemorySink {
    /// An empty sink for `key`.
    #[must_use]
    pub fn new(key: GraphKey) -> Self {
        Self {
            key: key.clone(),
            index: RwLock::new(GraphIndex::new(key)),
        }
    }

    /// The graph the sink holds.
    #[must_use]
    pub fn key(&self) -> &GraphKey {
        &self.key
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, GraphIndex> {
        self.index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, GraphIndex> {
        self.index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl GraphSink for InMemorySink {
    fn apply(&self, batch: &EdgeBatch) -> Result<SinkOutcome, ProjectionError> {
        let mut guard = self.write();
        let (outcome, _lines) = apply_into(&mut guard, &self.key, batch)?;
        Ok(outcome)
    }

    fn checkpoint(&self) -> u64 {
        self.read().watermark()
    }

    fn reset(&self) -> Result<(), ProjectionError> {
        *self.write() = GraphIndex::new(self.key.clone());
        Ok(())
    }
}

impl SinkReader for InMemorySink {
    fn key(&self) -> GraphKey {
        self.key.clone()
    }

    fn edges(&self) -> Vec<Edge> {
        self.read().edges().collect()
    }

    fn watermark(&self) -> u64 {
        self.read().watermark()
    }
}

/// A sink that keeps the graph in memory and appends a JSON-lines log, for tests and export.
#[derive(Debug)]
pub struct FileSink {
    key: GraphKey,
    path: PathBuf,
    index: RwLock<GraphIndex>,
    file: Mutex<File>,
}

impl FileSink {
    /// Opens (or creates) the log at `path` and rebuilds the graph from any lines already there.
    ///
    /// # Errors
    ///
    /// [`ProjectionError`] when the existing log cannot be read or replayed, or the file cannot
    /// be opened for appending.
    pub fn new(path: impl Into<PathBuf>, key: GraphKey) -> Result<Self, ProjectionError> {
        let path = path.into();
        let index = if path.exists() {
            replay(&path, key.clone())?
        } else {
            GraphIndex::new(key.clone())
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| ProjectionError::sink(format!("cannot open `{}`: {e}", path.display())))?;
        Ok(Self {
            key,
            path,
            index: RwLock::new(index),
            file: Mutex::new(file),
        })
    }

    /// The log's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, GraphIndex> {
        self.index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, GraphIndex> {
        self.index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn append(&self, lines: &[Json]) -> Result<(), ProjectionError> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for line in lines {
            let text = serde_json::to_string(line)
                .map_err(|e| ProjectionError::sink(format!("cannot encode a wire line: {e}")))?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .map_err(|e| ProjectionError::sink(format!("cannot write the log: {e}")))?;
        }
        file.flush()
            .map_err(|e| ProjectionError::sink(format!("cannot flush the log: {e}")))
    }
}

impl GraphSink for FileSink {
    fn apply(&self, batch: &EdgeBatch) -> Result<SinkOutcome, ProjectionError> {
        let (outcome, lines) = {
            let mut guard = self.write();
            apply_into(&mut guard, &self.key, batch)?
        };
        self.append(&lines)?;
        Ok(outcome)
    }

    fn checkpoint(&self) -> u64 {
        self.read().watermark()
    }

    fn reset(&self) -> Result<(), ProjectionError> {
        *self.write() = GraphIndex::new(self.key.clone());
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .map_err(|e| {
                ProjectionError::sink(format!("cannot truncate `{}`: {e}", self.path.display()))
            })?;
        Ok(())
    }
}

impl SinkReader for FileSink {
    fn key(&self) -> GraphKey {
        self.key.clone()
    }

    fn edges(&self) -> Vec<Edge> {
        self.read().edges().collect()
    }

    fn watermark(&self) -> u64 {
        self.read().watermark()
    }
}

/// Rebuilds a graph from a JSON-lines log: snapshot headers clear the graph and set its
/// watermark, edge lines add edges, and event lines apply in sequence.
///
/// # Errors
///
/// [`ProjectionError`] when a line is malformed or names another graph, or the result cannot form
/// a graph.
pub fn replay(path: &Path, key: GraphKey) -> Result<GraphIndex, ProjectionError> {
    let text = fs::read_to_string(path)
        .map_err(|e| ProjectionError::sink(format!("cannot read `{}`: {e}", path.display())))?;
    let mut edges: BTreeMap<EdgeKey, BTreeMap<String, Value>> = BTreeMap::new();
    let mut watermark = 0u64;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let json: Json = serde_json::from_str(line)
            .map_err(|e| ProjectionError::decode(format!("wire line {}: {e}", number + 1)))?;
        match kind_of(&json)? {
            "snapshot" => {
                watermark = snapshot_watermark(&json)?;
                edges.clear();
            }
            "edge" => {
                let edge = edge_from_json(&json)?;
                edges.insert(edge.key, edge.properties);
            }
            "event" => {
                let event = event_from_json(&json)?;
                if event.graph != key {
                    return Err(ProjectionError::WrongGraph {
                        expected: key.clone(),
                        got: event.graph,
                    });
                }
                if event.sequence <= watermark {
                    continue;
                }
                match event.op {
                    EdgeOp::Add | EdgeOp::Update => {
                        edges.insert(event.edge, event.properties);
                    }
                    EdgeOp::Remove => {
                        edges.remove(&event.edge);
                    }
                }
                watermark = event.sequence;
            }
            other => {
                return Err(ProjectionError::decode(format!(
                    "unknown wire line kind `{other}`"
                )));
            }
        }
    }
    let list: Vec<Edge> = edges
        .into_iter()
        .map(|(key, properties)| Edge { key, properties })
        .collect();
    GraphIndex::build(key, watermark, list).map_err(ProjectionError::from)
}
