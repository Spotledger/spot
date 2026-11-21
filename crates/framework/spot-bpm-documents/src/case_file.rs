//! The Case file: every document a Case touches, as business-graph edges.
//!
//! The Case file is not a table of its own. For every document a Case creates or changes it
//! records one edge `case -> document` through `spot-edge-sync`'s [`EdgeWriter`] — the only way
//! an edge is written (ADR-50) — carrying the node id, the operation and the opaque receipt as
//! edge properties. The Case occupies a `bpm_case` node with a deterministic id
//! ([`case_node`](crate::ids::case_node)); the document is its own node, so the business graph
//! answers "which documents did this Case touch" without a second store.
//!
//! [`CaseFile`] is the pure edge codec over any [`EdgeWriter`]; [`CaseFileStore`] is the small
//! synchronous facade the node port records through, and [`MemoryCaseFile`] is the reference
//! implementation over the in-memory writer.
//!
//! # Same transaction
//!
//! A backend's [`EdgeWriter`] is bound to the caller's transaction (WP-DAG-04), so a host that
//! runs the document operation and the Case-file write in one transaction gets them together.
//! The document engine's public lifecycle API opens its own transaction and does not yet expose
//! the transaction-bound participant seam (WP-DOC-04.T3); until it does, the reference port
//! records the Case-file edge immediately after the operation. See the crate's open questions.

use std::collections::BTreeSet;
use std::sync::Mutex;

use spot_app_api::prelude::{Name, RecordId, Version};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;
use spot_edge_sync::{Edge, EdgeFuture, EdgeWriter, MemoryEdgeWriter, PropValue};
use spot_graph_model::EdgeTypeId;

use crate::block::run_ready;
use crate::ids::case_node;
use crate::operation::Operation;

/// The canonical name of the `case -> document` edge type.
const EDGE_TYPE: &str = "bpm_case_file";

/// One document a Case touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The Case that touched the document.
    pub case: CaseId,
    /// The node that did it.
    pub node: NodeId,
    /// What it did.
    pub operation: Operation,
    /// The document's DocType (its storage name).
    pub doctype: Name,
    /// The document.
    pub document: RecordId,
    /// The document's version after the operation.
    pub version: Version,
    /// The inverse operation the node declared, when it declared one.
    pub compensation: Option<Operation>,
    /// The opaque receipt the lifecycle operation returned.
    pub receipt: serde_json::Value,
}

/// A document that a Case is expected to have touched, for the completeness check.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Touch {
    /// The document's DocType.
    pub doctype: Name,
    /// The document.
    pub document: RecordId,
}

impl Touch {
    /// The touch of one document.
    #[must_use]
    pub fn new(doctype: Name, document: RecordId) -> Self {
        Self { doctype, document }
    }
}

/// What the completeness check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completeness {
    /// Documents the Case was expected to touch and did not.
    pub missing: Vec<Touch>,
    /// Documents the Case touched and was not expected to.
    pub unexpected: Vec<Touch>,
}

impl Completeness {
    /// Whether the Case file lists exactly the expected documents.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.unexpected.is_empty()
    }
}

/// The pure codec: a Case's touched documents as edges, and back.
pub struct CaseFile;

impl CaseFile {
    /// Records one touched document as an edge, through `writer`.
    ///
    /// The edge is owned by the Case node, so a backend's bounded `scan_by_source` reads a
    /// Case's whole file. Writing the same `(case, document)` again updates the edge's
    /// properties (the latest node, operation and receipt).
    pub fn record<'a>(writer: &'a mut dyn EdgeWriter, entry: &'a Entry) -> EdgeFuture<'a, ()> {
        Box::pin(async move {
            let edge = edge_of(entry);
            writer.add(&edge).await
        })
    }

    /// The documents a Case has touched, in edge-key order.
    pub fn entries<'a>(
        writer: &'a mut dyn EdgeWriter,
        case: &'a CaseId,
    ) -> EdgeFuture<'a, Vec<Entry>> {
        Box::pin(async move {
            let source = case_node(case);
            let edges = writer.scan_by_source(&source).await?;
            Ok(edges
                .iter()
                .filter_map(|edge| entry_of(edge, case))
                .collect())
        })
    }

    /// Whether the Case file lists exactly the documents in `expected`.
    #[must_use]
    pub fn check(entries: &[Entry], expected: &[Touch]) -> Completeness {
        let have: BTreeSet<Touch> = entries
            .iter()
            .map(|entry| Touch::new(entry.doctype.clone(), entry.document))
            .collect();
        let want: BTreeSet<Touch> = expected.iter().cloned().collect();
        Completeness {
            missing: want.difference(&have).cloned().collect(),
            unexpected: have.difference(&want).cloned().collect(),
        }
    }
}

/// Where a Case's touched documents are recorded.
///
/// The node port records through this; the reference implementation is [`MemoryCaseFile`], and a
/// host supplies its own (bound to its transaction) in production.
pub trait CaseFileStore: Send + Sync {
    /// Records one touched document.
    ///
    /// # Errors
    /// [`CaseFileError`] when the edge could not be written.
    fn record(&self, entry: &Entry) -> Result<(), CaseFileError>;

    /// The documents a Case has touched.
    ///
    /// # Errors
    /// [`CaseFileError`] when the edges could not be read.
    fn entries(&self, case: &CaseId) -> Result<Vec<Entry>, CaseFileError>;
}

/// The in-memory [`CaseFileStore`]: the reference the tests use.
#[derive(Debug, Default)]
pub struct MemoryCaseFile {
    writer: Mutex<MemoryEdgeWriter>,
}

impl MemoryCaseFile {
    /// An empty Case file.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CaseFileStore for MemoryCaseFile {
    fn record(&self, entry: &Entry) -> Result<(), CaseFileError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| CaseFileError::new("the Case file is poisoned"))?;
        run_ready(CaseFile::record(&mut *writer, entry))
            .map_err(|error| CaseFileError::new(error.to_string()))
    }

    fn entries(&self, case: &CaseId) -> Result<Vec<Entry>, CaseFileError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| CaseFileError::new("the Case file is poisoned"))?;
        run_ready(CaseFile::entries(&mut *writer, case))
            .map_err(|error| CaseFileError::new(error.to_string()))
    }
}

/// Why a Case-file write or read failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseFileError {
    message: String,
}

impl CaseFileError {
    /// An error with a plain-language message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl core::fmt::Display for CaseFileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CaseFileError {}

/// The edge one touched document becomes.
fn edge_of(entry: &Entry) -> Edge {
    let from = case_node(&entry.case);
    let to = spot_storage_contract::NodeRef {
        entity: entry.doctype.clone(),
        id: entry.document,
    };
    Edge::new(EdgeTypeId::of(EDGE_TYPE), from, to)
        .with_property("node", PropValue::Text(entry.node.as_str().to_owned()))
        .with_property(
            "operation",
            PropValue::Text(entry.operation.token().to_owned()),
        )
        .with_property("version", PropValue::Int(version_i64(entry.version)))
        .with_property("receipt", PropValue::Json(entry.receipt.to_string()))
        .with_property(
            "compensation",
            PropValue::Text(
                entry
                    .compensation
                    .map_or(String::new(), |operation| operation.token().to_owned()),
            ),
        )
}

/// One entry from an edge, when it is a Case-file edge.
fn entry_of(edge: &Edge, case: &CaseId) -> Option<Entry> {
    if edge.edge_type != EdgeTypeId::of(EDGE_TYPE) {
        return None;
    }
    let node = text_property(edge, "node").and_then(|text| NodeId::new(text).ok())?;
    let operation = text_property(edge, "operation").and_then(Operation::parse)?;
    let version = match edge.properties.get("version") {
        Some(PropValue::Int(raw)) => u64::try_from(*raw).map(Version).ok()?,
        _ => Version::FIRST,
    };
    let receipt = match edge.properties.get("receipt") {
        Some(PropValue::Json(text)) => serde_json::from_str(text).ok()?,
        _ => serde_json::Value::Null,
    };
    let compensation = text_property(edge, "compensation")
        .filter(|text| !text.is_empty())
        .and_then(Operation::parse);
    Some(Entry {
        case: case.clone(),
        node,
        operation,
        doctype: edge.to.entity.clone(),
        document: edge.to.id,
        version,
        compensation,
        receipt,
    })
}

/// A text edge property.
fn text_property<'a>(edge: &'a Edge, name: &str) -> Option<&'a str> {
    match edge.properties.get(name) {
        Some(PropValue::Text(text)) => Some(text.as_str()),
        _ => None,
    }
}

/// A version as the integer an edge property carries.
fn version_i64(version: Version) -> i64 {
    i64::try_from(version.0).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use spot_edge_sync::MemoryEdgeWriter;

    use super::*;
    use crate::ids::record_id_for;

    fn entry(case: &CaseId, node: &str, doctype: &str, document: RecordId) -> Entry {
        Entry {
            case: case.clone(),
            node: NodeId::new(node).unwrap(),
            operation: Operation::CreateFrom,
            doctype: Name::new(doctype).unwrap(),
            document,
            version: Version(1),
            compensation: None,
            receipt: serde_json::json!({"record": {"version": 1}}),
        }
    }

    #[test]
    fn entries_round_trip_through_a_writer() {
        let case = CaseId::new("case1").unwrap();
        let document = record_id_for("doc1");
        let mut writer = MemoryEdgeWriter::new();
        run_ready(CaseFile::record(
            &mut writer,
            &entry(&case, "order", "sales_order", document),
        ))
        .unwrap();
        let entries = run_ready(CaseFile::entries(&mut writer, &case)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].document, document);
        assert_eq!(entries[0].doctype.as_str(), "sales_order");
    }

    #[test]
    fn the_completeness_check_reports_both_directions() {
        let case = CaseId::new("case1").unwrap();
        let a = record_id_for("a");
        let b = record_id_for("b");
        let entries = vec![entry(&case, "order", "sales_order", a)];
        let complete = CaseFile::check(
            &entries,
            &[Touch::new(Name::new("sales_order").unwrap(), a)],
        );
        assert!(complete.is_complete());
        let missing = CaseFile::check(
            &entries,
            &[Touch::new(Name::new("sales_order").unwrap(), b)],
        );
        assert_eq!(missing.missing.len(), 1);
        assert_eq!(missing.unexpected.len(), 1);
    }
}
