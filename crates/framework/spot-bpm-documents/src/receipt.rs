//! The request a Document node sends to the document engine, and the receipt it gets back.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, Value, Version};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;

use crate::operation::Operation;

/// One document operation, ready for the port to run.
///
/// It carries everything the port needs and nothing about storage: the operation, the DocType,
/// the document to change (for `update`, `submit` and `cancel`), the field values a mapping
/// produced, the version the caller expected, the principal, and the engine's idempotency key.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentRequest {
    /// What to do.
    pub operation: Operation,
    /// The DocType to act on (its storage name).
    pub doctype: Name,
    /// The document to change, for an operation that targets an existing document.
    pub document: Option<RecordId>,
    /// The field values the mapping produced.
    pub fields: BTreeMap<Name, Value>,
    /// The version the caller expects, when it knows one; the port loads the current one
    /// otherwise.
    pub expected: Option<Version>,
    /// The inverse operation the node declares, stored for WP-BPM-09's compensation path.
    pub compensation: Option<Operation>,
    /// A named service principal, overriding the Case's actor.
    pub principal: Option<String>,
    /// The Case the node runs in.
    pub case: CaseId,
    /// The node.
    pub node: NodeId,
    /// The engine's `(case, node, attempt)` key, as a string; it becomes the document operation's
    /// request key, so a replay returns the first result.
    pub key: String,
    /// The instant the engine stepped at; time is an argument.
    pub now: OffsetDateTime,
}

/// What a lifecycle operation did, as an opaque, serialisable receipt (ADR-080 section 6).
///
/// Until WP-DOC-12 returns real operation receipts from the document engine, this holds the
/// operation, the record id and version; the `effects` array is empty. The engine treats the
/// effects as opaque JSON and never interprets ledger or stock data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentReceipt {
    operation: Operation,
    doctype: Name,
    document: RecordId,
    version: Version,
    effects: Json,
}

impl DocumentReceipt {
    /// A receipt for one lifecycle operation.
    #[must_use]
    pub fn new(operation: Operation, doctype: Name, document: RecordId, version: Version) -> Self {
        Self {
            operation,
            doctype,
            document,
            version,
            effects: Json::Array(Vec::new()),
        }
    }

    /// The operation that produced the receipt.
    #[must_use]
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The DocType acted on.
    #[must_use]
    pub fn doctype(&self) -> &Name {
        &self.doctype
    }

    /// The record the operation touched.
    #[must_use]
    pub fn document(&self) -> RecordId {
        self.document
    }

    /// The version the record has after the operation.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// The opaque effect entries the lifecycle reported (empty until WP-DOC-12).
    #[must_use]
    pub fn effects(&self) -> &Json {
        &self.effects
    }

    /// The receipt as canonical JSON: the opaque value the Case file stores.
    #[must_use]
    pub fn receipt_json(&self) -> Json {
        json!({
            "operation": self.operation.token(),
            "record": {
                "doctype": self.doctype.as_str(),
                "id": self.document.to_string(),
                "version": self.version.0,
            },
            "effects": self.effects,
        })
    }

    /// The node's output: the operation's result with the opaque receipt inside it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        json!({
            "operation": self.operation.token(),
            "doctype": self.doctype.as_str(),
            "id": self.document.to_string(),
            "version": self.version.0,
            "receipt": self.receipt_json(),
        })
    }
}
